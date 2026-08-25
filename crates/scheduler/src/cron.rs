//! The cron dialect this scheduler defines: five whitespace-separated fields — minute, hour,
//! day-of-month, month, day-of-week — evaluated in UTC.
//!
//! [`Trigger::Cron`](aik_api::scheduler::Trigger::Cron) leaves the dialect to the
//! implementation on purpose, because there is no dialect every deployment agrees on. This one
//! is the traditional `cron(5)` five-field form: `*`, a single value, a range (`1-5`), a list
//! (`1,2,3`), a step (`*/15`, `1-30/5`), and combinations of those joined by commas. Field
//! names (`JAN`, `MON`, ...) are not accepted — numbers only — which keeps the parser to one
//! job rather than two.
//!
//! Fields are always UTC, never the host's local time zone: a schedule means the same instant
//! wherever the kernel runs, and never depends on a time zone database being present. A
//! deployment that wants a wall-clock time in some other zone converts it to UTC before writing
//! the expression.
//!
//! When both day-of-month and day-of-week are restricted (neither is `*`), traditional cron
//! matches a date that satisfies *either* — not both. That is surprising and not this module's
//! choice to relitigate; `cron(5)` has specified it for decades and expressions copied from
//! elsewhere rely on it.

use chrono::{DateTime, Datelike, Duration as ChronoDuration, Months, TimeZone, Timelike, Utc};

use aik_core::clock::Timestamp;

/// How far into the future [`CronSchedule::next_after`] will search before giving up on an
/// expression that can never match (`0 0 31 2 *`: the 31st of February).
const SEARCH_HORIZON_YEARS: i32 = 8;

/// One parsed field: which values in its range are included.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Field {
    included: Vec<bool>,
    is_wildcard: bool,
}

impl Field {
    fn parse(text: &str, min: u32, max: u32) -> Result<Self, String> {
        let mut included = vec![false; max as usize + 1];
        for part in text.split(',') {
            let (range, step) = match part.split_once('/') {
                Some((range, step)) => (
                    range,
                    step.parse::<u32>()
                        .map_err(|_| format!("invalid step in `{part}`"))?,
                ),
                None => (part, 1),
            };
            if step == 0 {
                return Err(format!("step of zero in `{part}`"));
            }
            let (start, end) = if range == "*" {
                (min, max)
            } else if let Some((a, b)) = range.split_once('-') {
                let a = a
                    .parse::<u32>()
                    .map_err(|_| format!("invalid value in `{part}`"))?;
                let b = b
                    .parse::<u32>()
                    .map_err(|_| format!("invalid value in `{part}`"))?;
                (a, b)
            } else {
                let v = range
                    .parse::<u32>()
                    .map_err(|_| format!("invalid value in `{part}`"))?;
                (v, v)
            };
            if start < min || end > max || start > end {
                return Err(format!("`{part}` is outside the range {min}-{max}"));
            }
            let mut v = start;
            while v <= end {
                included[v as usize] = true;
                v += step;
            }
        }
        Ok(Self {
            included,
            is_wildcard: text == "*",
        })
    }

    fn contains(&self, value: u32) -> bool {
        self.included.get(value as usize).copied().unwrap_or(false)
    }
}

/// A parsed cron expression, ready to find its own next occurrence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CronSchedule {
    minute: Field,
    hour: Field,
    day_of_month: Field,
    month: Field,
    day_of_week: Field,
}

impl CronSchedule {
    /// Parses a five-field expression, or explains why it does not fit this dialect.
    pub(crate) fn parse(expression: &str) -> Result<Self, String> {
        let fields: Vec<&str> = expression.split_whitespace().collect();
        let [minute, hour, day_of_month, month, day_of_week] = fields.as_slice() else {
            return Err(format!(
                "expected 5 fields (minute hour day-of-month month day-of-week), found {}",
                fields.len()
            ));
        };
        // `7` is a traditional alias for Sunday alongside `0`; fold it in rather than carrying
        // two representations of the same day through the rest of the matcher.
        let mut day_of_week = Field::parse(day_of_week, 0, 7)?;
        if day_of_week.contains(7) {
            day_of_week.included[0] = true;
        }
        Ok(Self {
            minute: Field::parse(minute, 0, 59)?,
            hour: Field::parse(hour, 0, 23)?,
            day_of_month: Field::parse(day_of_month, 1, 31)?,
            month: Field::parse(month, 1, 12)?,
            day_of_week,
        })
    }

    fn day_matches(&self, date: DateTime<Utc>) -> bool {
        let dom_ok = self.day_of_month.contains(date.day());
        let dow_ok = self
            .day_of_week
            .contains(date.weekday().num_days_from_sunday());
        match (self.day_of_month.is_wildcard, self.day_of_week.is_wildcard) {
            (true, true) => true,
            (true, false) => dow_ok,
            (false, true) => dom_ok,
            (false, false) => dom_ok || dow_ok,
        }
    }

    /// The first occurrence strictly after `after`, or `None` if the expression cannot match
    /// within [`SEARCH_HORIZON_YEARS`] — the honest answer for one that can never match at all.
    pub(crate) fn next_after(&self, after: Timestamp) -> Option<Timestamp> {
        let after = Utc
            .timestamp_millis_opt(i64::try_from(after.as_millis()).ok()?)
            .single()?;
        let horizon = after.year() + SEARCH_HORIZON_YEARS;
        let mut candidate = truncate_to_minute(after) + ChronoDuration::minutes(1);

        loop {
            if candidate.year() > horizon {
                return None;
            }
            if !self.month.contains(candidate.month()) {
                candidate = start_of_month(candidate)?.checked_add_months(Months::new(1))?;
                continue;
            }
            if !self.day_matches(candidate) {
                candidate = start_of_day(candidate) + ChronoDuration::days(1);
                continue;
            }
            if !self.hour.contains(candidate.hour()) {
                candidate = start_of_hour(candidate) + ChronoDuration::hours(1);
                continue;
            }
            if !self.minute.contains(candidate.minute()) {
                candidate += ChronoDuration::minutes(1);
                continue;
            }
            let millis = u64::try_from(candidate.timestamp_millis()).ok()?;
            return Some(Timestamp::from_millis(millis));
        }
    }
}

fn truncate_to_minute(dt: DateTime<Utc>) -> DateTime<Utc> {
    dt.with_second(0).unwrap().with_nanosecond(0).unwrap()
}

fn start_of_hour(dt: DateTime<Utc>) -> DateTime<Utc> {
    truncate_to_minute(dt).with_minute(0).unwrap()
}

fn start_of_day(dt: DateTime<Utc>) -> DateTime<Utc> {
    start_of_hour(dt).with_hour(0).unwrap()
}

fn start_of_month(dt: DateTime<Utc>) -> Option<DateTime<Utc>> {
    start_of_day(dt).with_day(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(millis: u64) -> Timestamp {
        Timestamp::from_millis(millis)
    }

    fn ymdhm(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> Timestamp {
        let dt = Utc.with_ymd_and_hms(y, mo, d, h, mi, 0).single().unwrap();
        Timestamp::from_millis(u64::try_from(dt.timestamp_millis()).unwrap())
    }

    #[test]
    fn rejects_the_wrong_number_of_fields() {
        assert!(CronSchedule::parse("* * * *").is_err());
        assert!(CronSchedule::parse("* * * * * *").is_err());
    }

    #[test]
    fn rejects_a_value_outside_its_range() {
        assert!(
            CronSchedule::parse("60 * * * *").is_err(),
            "minute goes to 59"
        );
        assert!(
            CronSchedule::parse("* 24 * * *").is_err(),
            "hour goes to 23"
        );
        assert!(
            CronSchedule::parse("* * 32 * *").is_err(),
            "day-of-month goes to 31"
        );
        assert!(
            CronSchedule::parse("* * * 13 *").is_err(),
            "month goes to 12"
        );
    }

    #[test]
    fn every_minute_fires_the_very_next_minute() {
        let schedule = CronSchedule::parse("* * * * *").unwrap();
        assert_eq!(
            schedule.next_after(ymdhm(2026, 1, 1, 12, 30)),
            Some(ymdhm(2026, 1, 1, 12, 31))
        );
    }

    #[test]
    fn a_daily_expression_finds_the_next_day_when_todays_time_has_passed() {
        let schedule = CronSchedule::parse("0 3 * * *").unwrap();
        assert_eq!(
            schedule.next_after(ymdhm(2026, 1, 1, 4, 0)),
            Some(ymdhm(2026, 1, 2, 3, 0))
        );
    }

    #[test]
    fn a_daily_expression_finds_later_today_when_the_time_has_not_passed_yet() {
        let schedule = CronSchedule::parse("0 3 * * *").unwrap();
        assert_eq!(
            schedule.next_after(ymdhm(2026, 1, 1, 1, 0)),
            Some(ymdhm(2026, 1, 1, 3, 0))
        );
    }

    #[test]
    fn a_step_expression_matches_every_nth_value() {
        let schedule = CronSchedule::parse("*/15 * * * *").unwrap();
        assert_eq!(
            schedule.next_after(ymdhm(2026, 1, 1, 12, 1)),
            Some(ymdhm(2026, 1, 1, 12, 15))
        );
    }

    #[test]
    fn month_rollover_crosses_into_the_next_year() {
        let schedule = CronSchedule::parse("0 0 1 1 *").unwrap();
        assert_eq!(
            schedule.next_after(ymdhm(2026, 3, 1, 0, 0)),
            Some(ymdhm(2027, 1, 1, 0, 0))
        );
    }

    #[test]
    fn day_of_month_and_day_of_week_are_ored_when_both_are_restricted() {
        // The 1st of January 2026 is a Thursday; day-of-week asks for Monday (1).
        // Traditional cron fires on either, so the 1st still matches.
        let schedule = CronSchedule::parse("0 0 1 * 1").unwrap();
        assert_eq!(
            schedule.next_after(ymdhm(2025, 12, 31, 0, 0)),
            Some(ymdhm(2026, 1, 1, 0, 0))
        );
    }

    #[test]
    fn day_of_week_seven_is_an_alias_for_sunday() {
        let zero = CronSchedule::parse("0 0 * * 0").unwrap();
        let seven = CronSchedule::parse("0 0 * * 7").unwrap();
        let after = ymdhm(2026, 1, 1, 0, 0);
        assert_eq!(zero.next_after(after), seven.next_after(after));
    }

    #[test]
    fn an_expression_that_can_never_match_gives_up_rather_than_searching_forever() {
        let schedule = CronSchedule::parse("0 0 31 2 *").unwrap();
        assert_eq!(schedule.next_after(at(0)), None);
    }
}
