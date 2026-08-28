//! The accounting windows a ceiling is measured over.
//!
//! A period turns an instant into two things: the **key** the counter for that instant is
//! stored under, and when that counter stops accumulating. Everything is UTC, because a
//! ledger shared by a daemon, a terminal and a schedule cannot depend on which machine's
//! timezone was in effect when a turn was taken.
//!
//! # Why keys are strings
//!
//! `day:2026-08-28` sorts chronologically as text, is readable in a database dump, and says
//! which period produced it. That last part is what makes a window key safe to store next to
//! others: two periods can never collide on one key, so a daily and a monthly ceiling on the
//! same principal count independently without either knowing the other exists.
//!
//! Sorting chronologically is not cosmetic either. It is what lets a write prune the windows
//! that have closed with one range operation — see [`QuotaPeriod::prefix`].

use aik_core::clock::Timestamp;
use aik_core::{Error, Result};
use chrono::{
    DateTime, Datelike, Days, Duration as ChronoDuration, Months, TimeZone, Timelike, Utc,
};
use serde::{Deserialize, Serialize};

/// How often a counter starts again.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuotaPeriod {
    /// The UTC hour.
    Hour,
    /// The UTC calendar day.
    Day,
    /// The ISO-8601 week, Monday to Sunday, in UTC.
    Week,
    /// The UTC calendar month.
    Month,
    /// Never: one counter, for the lifetime of the ledger.
    Total,
}

/// One accounting window: what to count under, and when counting stops.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Window {
    /// The key this window's counter is stored under, e.g. `day:2026-08-28`.
    pub key: String,
    /// When the window closes, or `None` for [`QuotaPeriod::Total`].
    pub ends: Option<Timestamp>,
}

impl QuotaPeriod {
    /// The prefix every key of this period starts with.
    ///
    /// Two uses, and the second is the reason it exists as its own function: it namespaces
    /// keys so periods cannot collide, and it bounds a range covering exactly this period's
    /// windows. Since keys sort chronologically within a prefix, `prefix ..= current key`
    /// is precisely "this period's windows, up to and including the open one", which is how
    /// a write drops the closed ones without scanning anything else.
    pub const fn prefix(self) -> &'static str {
        match self {
            Self::Hour => "hour:",
            Self::Day => "day:",
            Self::Week => "week:",
            Self::Month => "month:",
            Self::Total => "total",
        }
    }

    /// The window `at` falls in.
    pub fn window(self, at: Timestamp) -> Result<Window> {
        if self == Self::Total {
            return Ok(Window {
                key: Self::Total.prefix().to_owned(),
                ends: None,
            });
        }

        let now = utc(at)?;
        let (key, ends) = match self {
            Self::Hour => (
                now.format("hour:%Y-%m-%dT%H").to_string(),
                truncate_to_hour(now).checked_add_signed(ChronoDuration::hours(1)),
            ),
            Self::Day => (
                now.format("day:%Y-%m-%d").to_string(),
                start_of_day(now).checked_add_days(Days::new(1)),
            ),
            // `%G-W%V` is the ISO week-numbering year and week, which is what makes the key
            // agree with the boundary below: in the days either side of New Year those two
            // disagree with the calendar year, and a key built from `%Y` would put one ISO
            // week under two different counters.
            Self::Week => (
                now.format("week:%G-W%V").to_string(),
                start_of_week(now).checked_add_days(Days::new(7)),
            ),
            Self::Month => (
                now.format("month:%Y-%m").to_string(),
                start_of_month(now).checked_add_months(Months::new(1)),
            ),
            Self::Total => unreachable!("handled above"),
        };

        let ends = ends.ok_or_else(|| {
            Error::other(format!(
                "the end of the {self:?} window containing {}ms is not representable",
                at.as_millis()
            ))
        })?;
        Ok(Window {
            key,
            ends: Some(timestamp(ends)),
        })
    }
}

impl std::fmt::Display for QuotaPeriod {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Hour => "hour",
            Self::Day => "day",
            Self::Week => "week",
            Self::Month => "month",
            Self::Total => "total",
        })
    }
}

/// Converts a kernel timestamp into a UTC date-time, failing rather than clamping.
///
/// A clamped instant would put a charge in the wrong window and, for the maximum, in a
/// window that never closes. Nothing here has to guess what the caller meant: a clock that
/// reports an unrepresentable instant is broken, and a quota that fails closed is the
/// documented behaviour.
fn utc(at: Timestamp) -> Result<DateTime<Utc>> {
    i64::try_from(at.as_millis())
        .ok()
        .and_then(DateTime::from_timestamp_millis)
        .ok_or_else(|| {
            Error::other(format!(
                "the clock reported {}ms since the epoch, which is not a representable instant",
                at.as_millis()
            ))
        })
}

/// Converts back, saturating: every value produced here comes from a window boundary
/// derived from an instant that already converted, so the saturation is unreachable and is
/// present only because the conversion is fallible in the type system.
fn timestamp(at: DateTime<Utc>) -> Timestamp {
    Timestamp::from_millis(u64::try_from(at.timestamp_millis()).unwrap_or(u64::MAX))
}

fn truncate_to_hour(at: DateTime<Utc>) -> DateTime<Utc> {
    at.with_minute(0)
        .and_then(|at| at.with_second(0))
        .and_then(|at| at.with_nanosecond(0))
        .unwrap_or(at)
}

fn start_of_day(at: DateTime<Utc>) -> DateTime<Utc> {
    Utc.from_utc_datetime(&at.date_naive().and_time(chrono::NaiveTime::MIN))
}

fn start_of_week(at: DateTime<Utc>) -> DateTime<Utc> {
    let monday = at
        .date_naive()
        .checked_sub_days(Days::new(u64::from(at.weekday().num_days_from_monday())))
        .unwrap_or_else(|| at.date_naive());
    Utc.from_utc_datetime(&monday.and_time(chrono::NaiveTime::MIN))
}

fn start_of_month(at: DateTime<Utc>) -> DateTime<Utc> {
    let first = at
        .date_naive()
        .with_day(1)
        .unwrap_or_else(|| at.date_naive());
    Utc.from_utc_datetime(&first.and_time(chrono::NaiveTime::MIN))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2026-08-28T14:35:12Z, a Friday.
    const FRIDAY: u64 = 1_787_927_712_000;

    fn window(period: QuotaPeriod, at: u64) -> Window {
        period.window(Timestamp::from_millis(at)).unwrap()
    }

    #[test]
    fn keys_name_their_period_and_their_window() {
        assert_eq!(window(QuotaPeriod::Hour, FRIDAY).key, "hour:2026-08-28T14");
        assert_eq!(window(QuotaPeriod::Day, FRIDAY).key, "day:2026-08-28");
        assert_eq!(window(QuotaPeriod::Week, FRIDAY).key, "week:2026-W35");
        assert_eq!(window(QuotaPeriod::Month, FRIDAY).key, "month:2026-08");
        assert_eq!(window(QuotaPeriod::Total, FRIDAY).key, "total");
    }

    #[test]
    fn every_key_starts_with_its_periods_prefix() {
        for period in [
            QuotaPeriod::Hour,
            QuotaPeriod::Day,
            QuotaPeriod::Week,
            QuotaPeriod::Month,
            QuotaPeriod::Total,
        ] {
            assert!(
                window(period, FRIDAY).key.starts_with(period.prefix()),
                "{period} keys must be prunable by prefix"
            );
        }
    }

    #[test]
    fn keys_sort_chronologically_within_a_period() {
        let a = window(QuotaPeriod::Day, FRIDAY).key;
        let b = window(QuotaPeriod::Day, FRIDAY + 24 * 60 * 60 * 1_000).key;
        assert!(a < b, "{a} should sort before {b}");

        let a = window(QuotaPeriod::Month, FRIDAY).key;
        let b = window(QuotaPeriod::Month, FRIDAY + 40 * 24 * 60 * 60 * 1_000).key;
        assert!(a < b, "{a} should sort before {b}");
    }

    #[test]
    fn a_window_ends_at_the_start_of_the_next_one() {
        let day = window(QuotaPeriod::Day, FRIDAY);
        assert_eq!(
            window(QuotaPeriod::Day, day.ends.unwrap().as_millis()).key,
            "day:2026-08-29"
        );

        let hour = window(QuotaPeriod::Hour, FRIDAY);
        assert_eq!(
            window(QuotaPeriod::Hour, hour.ends.unwrap().as_millis()).key,
            "hour:2026-08-28T15"
        );
    }

    #[test]
    fn a_week_runs_monday_to_monday() {
        let week = window(QuotaPeriod::Week, FRIDAY);
        let ends = week.ends.unwrap();
        // The Monday after the Friday above, at midnight UTC.
        assert_eq!(
            DateTime::from_timestamp_millis(ends.as_millis() as i64)
                .unwrap()
                .format("%Y-%m-%d %H:%M:%S")
                .to_string(),
            "2026-08-31 00:00:00"
        );
        // Every instant inside the window shares one key.
        assert_eq!(
            window(QuotaPeriod::Week, ends.as_millis() - 1).key,
            week.key
        );
        assert_ne!(window(QuotaPeriod::Week, ends.as_millis()).key, week.key);
    }

    #[test]
    fn a_month_ends_at_the_first_of_the_next_one() {
        let month = window(QuotaPeriod::Month, FRIDAY);
        assert_eq!(
            window(QuotaPeriod::Month, month.ends.unwrap().as_millis()).key,
            "month:2026-09"
        );
    }

    #[test]
    fn the_total_window_never_ends() {
        assert_eq!(window(QuotaPeriod::Total, FRIDAY).ends, None);
        assert_eq!(window(QuotaPeriod::Total, 0).key, "total");
    }

    #[test]
    fn an_unrepresentable_instant_is_an_error_rather_than_a_clamp() {
        let error = QuotaPeriod::Day.window(Timestamp::from_millis(u64::MAX));
        assert!(
            error.is_err(),
            "a broken clock must not silently pick a window"
        );
    }

    #[test]
    fn periods_round_trip_through_configuration_spelling() {
        for (text, period) in [
            ("hour", QuotaPeriod::Hour),
            ("day", QuotaPeriod::Day),
            ("week", QuotaPeriod::Week),
            ("month", QuotaPeriod::Month),
            ("total", QuotaPeriod::Total),
        ] {
            let parsed: QuotaPeriod = serde_json::from_value(serde_json::json!(text)).unwrap();
            assert_eq!(parsed, period);
            assert_eq!(period.to_string(), text);
        }
    }
}
