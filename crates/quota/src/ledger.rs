//! The counters themselves, and the volatile backend.
//!
//! A ledger stores one row per (subject, window) and knows nothing about rules, prices or
//! principals: it adds numbers and reads them back. Every decision about *which* rows to
//! touch and what they mean belongs to [`LimitedQuotaGuard`](crate::LimitedQuotaGuard), so
//! the two backends here and in [`crate::persistent`] cannot drift on anything more
//! interesting than storage.

use std::collections::BTreeMap;
use std::sync::Mutex;

use aik_api::quota::QuotaDimension;
use aik_core::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::period::QuotaPeriod;

/// What one subject has spent in one window.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Counters {
    /// Model turns taken.
    pub turns: u64,
    /// Tokens sent.
    pub input_tokens: u64,
    /// Tokens received.
    pub output_tokens: u64,
    /// What those tokens were priced at, in micros.
    pub cost_micros: u64,
}

impl Counters {
    /// Adds two sets of counters, saturating.
    ///
    /// Saturation rather than wrapping, because a counter that wraps hands a subject an
    /// unbounded budget at the exact moment it has spent the most.
    #[must_use]
    pub fn saturating_add(self, other: Self) -> Self {
        Self {
            turns: self.turns.saturating_add(other.turns),
            input_tokens: self.input_tokens.saturating_add(other.input_tokens),
            output_tokens: self.output_tokens.saturating_add(other.output_tokens),
            cost_micros: self.cost_micros.saturating_add(other.cost_micros),
        }
    }

    /// What this row says about one dimension.
    pub fn get(&self, dimension: QuotaDimension) -> u64 {
        match dimension {
            QuotaDimension::Turns => self.turns,
            QuotaDimension::InputTokens => self.input_tokens,
            QuotaDimension::OutputTokens => self.output_tokens,
            QuotaDimension::TotalTokens => self.input_tokens.saturating_add(self.output_tokens),
            QuotaDimension::CostMicros => self.cost_micros,
        }
    }

    /// Whether anything has been counted at all.
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// Where usage counters live.
///
/// # What an implementation must guarantee
///
/// * [`add`](UsageLedger::add) is atomic against concurrent adds to the same row. A quota
///   enforced by a lost update is not enforced.
/// * A row that has never been written reads as [`Counters::default`], not as an error: an
///   unspent subject is the ordinary case, not a missing one.
/// * Windows that have closed may be dropped, and only those. A ledger is an enforcement
///   counter and not a record of what happened — that is the audit trail's job — so it is
///   bounded by how many subjects and periods a deployment has, never by how long it has
///   been running.
#[async_trait]
pub trait UsageLedger: Send + Sync + std::fmt::Debug + 'static {
    /// Reads one row, which may never have been written.
    async fn read(&self, subject: &str, window: &str) -> Result<Counters>;

    /// Adds `delta` to one row, and drops this subject's closed windows of the same period.
    async fn add(
        &self,
        subject: &str,
        period: QuotaPeriod,
        window: &str,
        delta: Counters,
    ) -> Result<()>;
}

/// A [`UsageLedger`] that lives as long as the process.
///
/// The right pairing for an `--ephemeral` deployment, and the implementation the durable one
/// is measured against. It is a real limit while the process runs; what it does not survive
/// is a restart, so a deployment that must not be able to reset its own budget by restarting
/// wants [`RedbUsageLedger`](crate::RedbUsageLedger).
#[derive(Debug, Default)]
pub struct InMemoryUsageLedger {
    /// Rows keyed by (subject, window). Ordered so that a period's closed windows are one
    /// contiguous range, exactly as in the durable backend.
    ///
    /// A plain mutex rather than an async one: every operation is a short map access with no
    /// await inside it, so the lock is never held across a suspension point.
    rows: Mutex<BTreeMap<(String, String), Counters>>,
}

impl InMemoryUsageLedger {
    /// Creates an empty ledger.
    pub fn new() -> Self {
        Self::default()
    }

    /// How many rows are held. For diagnostics and tests.
    pub fn row_count(&self) -> usize {
        self.rows.lock().expect("the usage ledger lock").len()
    }
}

#[async_trait]
impl UsageLedger for InMemoryUsageLedger {
    async fn read(&self, subject: &str, window: &str) -> Result<Counters> {
        Ok(self
            .rows
            .lock()
            .expect("the usage ledger lock")
            .get(&(subject.to_owned(), window.to_owned()))
            .copied()
            .unwrap_or_default())
    }

    async fn add(
        &self,
        subject: &str,
        period: QuotaPeriod,
        window: &str,
        delta: Counters,
    ) -> Result<()> {
        let mut rows = self.rows.lock().expect("the usage ledger lock");
        let key = (subject.to_owned(), window.to_owned());
        let updated = rows
            .get(&key)
            .copied()
            .unwrap_or_default()
            .saturating_add(delta);
        rows.insert(key, updated);

        let closed: Vec<(String, String)> = rows
            .range(
                (subject.to_owned(), period.prefix().to_owned())
                    ..(subject.to_owned(), window.to_owned()),
            )
            .map(|(key, _)| key.clone())
            .collect();
        for key in closed {
            rows.remove(&key);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn counters(turns: u64, input: u64, output: u64, cost: u64) -> Counters {
        Counters {
            turns,
            input_tokens: input,
            output_tokens: output,
            cost_micros: cost,
        }
    }

    #[test]
    fn totals_are_derived_and_saturate() {
        let row = counters(1, u64::MAX, 10, 0);
        assert_eq!(row.get(QuotaDimension::TotalTokens), u64::MAX);
        assert_eq!(row.get(QuotaDimension::InputTokens), u64::MAX);
        assert_eq!(row.get(QuotaDimension::Turns), 1);
    }

    #[test]
    fn adding_saturates_rather_than_wrapping() {
        let sum = counters(u64::MAX, 0, 0, 0).saturating_add(counters(1, 0, 0, 0));
        assert_eq!(
            sum.turns,
            u64::MAX,
            "a wrapped counter is an unbounded budget"
        );
    }

    #[tokio::test]
    async fn an_unwritten_row_reads_as_nothing_spent() {
        let ledger = InMemoryUsageLedger::new();
        assert!(
            ledger
                .read("alice", "day:2026-08-28")
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn adds_accumulate_within_a_window() {
        let ledger = InMemoryUsageLedger::new();
        for _ in 0..3 {
            ledger
                .add(
                    "alice",
                    QuotaPeriod::Day,
                    "day:2026-08-28",
                    counters(1, 10, 2, 5),
                )
                .await
                .unwrap();
        }
        assert_eq!(
            ledger.read("alice", "day:2026-08-28").await.unwrap(),
            counters(3, 30, 6, 15)
        );
    }

    #[tokio::test]
    async fn subjects_and_periods_are_counted_apart() {
        let ledger = InMemoryUsageLedger::new();
        ledger
            .add(
                "alice",
                QuotaPeriod::Day,
                "day:2026-08-28",
                counters(1, 0, 0, 0),
            )
            .await
            .unwrap();
        ledger
            .add(
                "bob",
                QuotaPeriod::Day,
                "day:2026-08-28",
                counters(5, 0, 0, 0),
            )
            .await
            .unwrap();
        ledger
            .add(
                "alice",
                QuotaPeriod::Month,
                "month:2026-08",
                counters(9, 0, 0, 0),
            )
            .await
            .unwrap();

        assert_eq!(
            ledger.read("alice", "day:2026-08-28").await.unwrap().turns,
            1
        );
        assert_eq!(ledger.read("bob", "day:2026-08-28").await.unwrap().turns, 5);
        assert_eq!(
            ledger.read("alice", "month:2026-08").await.unwrap().turns,
            9
        );
    }

    #[tokio::test]
    async fn a_closed_window_is_dropped_but_other_periods_survive() {
        let ledger = InMemoryUsageLedger::new();
        ledger
            .add(
                "alice",
                QuotaPeriod::Day,
                "day:2026-08-27",
                counters(1, 0, 0, 0),
            )
            .await
            .unwrap();
        ledger
            .add(
                "alice",
                QuotaPeriod::Month,
                "month:2026-08",
                counters(1, 0, 0, 0),
            )
            .await
            .unwrap();
        ledger
            .add(
                "bob",
                QuotaPeriod::Day,
                "day:2026-08-27",
                counters(1, 0, 0, 0),
            )
            .await
            .unwrap();

        ledger
            .add(
                "alice",
                QuotaPeriod::Day,
                "day:2026-08-28",
                counters(1, 0, 0, 0),
            )
            .await
            .unwrap();

        assert!(
            ledger
                .read("alice", "day:2026-08-27")
                .await
                .unwrap()
                .is_empty(),
            "yesterday's counter is not what today is measured against"
        );
        assert_eq!(
            ledger.read("alice", "day:2026-08-28").await.unwrap().turns,
            1
        );
        assert_eq!(
            ledger.read("alice", "month:2026-08").await.unwrap().turns,
            1,
            "a daily write must not reclaim the monthly counter it sits inside"
        );
        assert_eq!(
            ledger.read("bob", "day:2026-08-27").await.unwrap().turns,
            1,
            "one subject's write must not touch another's rows"
        );
        assert_eq!(ledger.row_count(), 3);
    }

    #[tokio::test]
    async fn the_total_window_is_never_pruned() {
        let ledger = InMemoryUsageLedger::new();
        for _ in 0..2 {
            ledger
                .add("alice", QuotaPeriod::Total, "total", counters(1, 0, 0, 0))
                .await
                .unwrap();
        }
        assert_eq!(ledger.read("alice", "total").await.unwrap().turns, 2);
        assert_eq!(ledger.row_count(), 1);
    }
}
