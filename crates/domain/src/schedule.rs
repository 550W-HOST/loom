//! When an automation fires: five-field cron expressions read in a timezone.
//!
//! Two established crates do the parts that are easy to get subtly wrong:
//! `croner` parses and evaluates the expression, and `chrono-tz` supplies the
//! timezone database. The database is compiled into the binary, so a scheduled
//! run in a named zone works in the `FROM scratch` server image too — there is
//! nothing to mount and no host `tzdata` to depend on.
//!
//! This module is the adapter between them and the automation contract:
//!
//! * **The dialect is five fields.** `croner` also accepts a leading seconds
//!   field; the contract's expressions are `minute hour day-of-month month
//!   day-of-week`, and a six-field expression is rejected here rather than
//!   quietly reinterpreted.
//! * **A zone must exist.** `chrono-tz` knows the IANA database, so a name that
//!   is not a zone is rejected at the edge instead of being accepted as syntax
//!   and never firing.
//! * **The answer is always in the zone.** Occurrences are computed against a
//!   `DateTime<Tz>`, so day-of-week and the time of day mean what they mean
//!   where the automation lives.
//!
//! `croner` owns the DST rules, and the tests here pin them so a dependency
//! upgrade that changes them fails loudly:
//!
//! * a local time the clock **skipped forward** over fires at the next instant
//!   that exists, so a daily schedule fires once that day instead of skipping
//!   it;
//! * a local time the clock **fell back** over fires once, on the earlier pass
//!   — the repeated hour does not produce a second run.
//!
//! Both matter more here than in a batch job: the scheduler claims a window and
//! records it, and firing twice for one window would be a duplicate run while
//! skipping a day would be a missing one. Every returned instant is strictly
//! after the instant asked about, which is what keeps a restart from re-firing
//! the window it already fired.
//!
//! # The dialect
//!
//! `croner`'s, by admission: `*`, lists, ranges, `*/S` and `X-Y/S` steps, the
//! three-letter month and day names, `?` in the two day fields, `L` and `#` in
//! the day fields. A *bare* `N/S` step (`5/15`) is refused with croner's own
//! message, which is one place this is narrower than the reference plugin's
//! evaluator — the product UI writes `*/S` steps, and a range step is the
//! spelling croner accepts for the rest.

use std::fmt;
use std::str::FromStr;

use chrono::{DateTime, TimeZone, Utc};
use chrono_tz::Tz;
use croner::Cron;

/// An expression croner or the zone lookup refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ScheduleError {
    /// The expression did not have five whitespace-separated fields.
    FieldCount(usize),
    /// The expression did not parse.
    Expression(String),
    /// The timezone name is not in the IANA database.
    UnknownZone(String),
}

impl fmt::Display for ScheduleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::FieldCount(count) => write!(
                f,
                "must have exactly 5 fields (minute hour day-of-month month day-of-week), found \
                 {count}"
            ),
            Self::Expression(message) => write!(f, "is not a valid expression: {message}"),
            Self::UnknownZone(name) => write!(f, "{name:?} is not a known timezone"),
        }
    }
}

impl std::error::Error for ScheduleError {}

/// A cron expression together with the zone it is read in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Schedule {
    cron: Cron,
    zone: Tz,
    expression: String,
}

impl Schedule {
    /// Parses an expression and resolves the zone it names.
    pub fn parse(expression: &str, timezone: &str) -> Result<Self, ScheduleError> {
        let fields = expression.split_whitespace().count();
        if fields != 5 {
            return Err(ScheduleError::FieldCount(fields));
        }
        let cron = Cron::from_str(expression)
            .map_err(|error| ScheduleError::Expression(error.to_string()))?;
        let zone = resolve_zone(timezone)?;
        Ok(Self {
            cron,
            zone,
            expression: expression.to_owned(),
        })
    }

    /// The next instant (epoch milliseconds) this expression fires at, strictly
    /// after `after_ms`.
    ///
    /// `None` means there is no such instant: croner's own search bound was
    /// reached (a date that can never match, like 31 February).
    pub fn next_after(&self, after_ms: u64) -> Option<u64> {
        let after = self.zoned(after_ms);
        let next = self.cron.find_next_occurrence(&after, false).ok()?;
        u64::try_from(next.timestamp_millis()).ok()
    }

    /// The instant (epoch milliseconds) this expression fired at on or before
    /// `at_ms`, when there is one.
    ///
    /// Used by diagnostics: "the schedule that never fired" is a question about
    /// the past as much as the future.
    pub fn previous_at_or_before(&self, at_ms: u64) -> Option<u64> {
        let at = self.zoned(at_ms);
        let previous = self.cron.find_previous_occurrence(&at, true).ok()?;
        u64::try_from(previous.timestamp_millis()).ok()
    }

    /// The zone this schedule is read in.
    pub fn zone(&self) -> Tz {
        self.zone
    }

    /// The expression, as it was given.
    pub fn expression(&self) -> &str {
        &self.expression
    }

    fn zoned(&self, at_ms: u64) -> DateTime<Tz> {
        let seconds = i64::try_from(at_ms / 1_000).unwrap_or(i64::MAX);
        let nanos = i64::try_from((at_ms % 1_000) * 1_000_000).unwrap_or(0);
        Utc.timestamp_opt(seconds, nanos as u32)
            .single()
            .map_or_else(
                || {
                    Utc.timestamp_opt(0, 0)
                        .single()
                        .expect("the epoch exists")
                        .with_timezone(&self.zone)
                },
                |utc| utc.with_timezone(&self.zone),
            )
    }
}

/// Resolves an IANA timezone name.
pub fn resolve_zone(timezone: &str) -> Result<Tz, ScheduleError> {
    Tz::from_str(timezone).map_err(|_| ScheduleError::UnknownZone(timezone.to_owned()))
}

/// Validates an expression without binding it to a zone.
///
/// A trigger's two fields are validated separately because they are separate
/// fields in the contract: an expression is checked when it arrives, and the
/// zone it will be read in is checked alongside it.
pub fn validate_expression(expression: &str) -> Result<(), ScheduleError> {
    let fields = expression.split_whitespace().count();
    if fields != 5 {
        return Err(ScheduleError::FieldCount(fields));
    }
    Cron::from_str(expression)
        .map(|_| ())
        .map_err(|error| ScheduleError::Expression(error.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Epoch milliseconds of a UTC instant, for readable assertions.
    fn utc_ms(year: i32, month: u32, day: u32, hour: u32, minute: u32) -> u64 {
        Utc.with_ymd_and_hms(year, month, day, hour, minute, 0)
            .single()
            .expect("a real instant")
            .timestamp_millis() as u64
    }

    /// The same instant expressed in a zone, to state an assertion in local
    /// terms and still compare instants.
    fn local_ms(zone: Tz, year: i32, month: u32, day: u32, hour: u32, minute: u32) -> u64 {
        zone.with_ymd_and_hms(year, month, day, hour, minute, 0)
            .single()
            .expect("a real local instant")
            .timestamp_millis() as u64
    }

    #[test]
    fn a_schedule_is_read_in_its_zone_not_in_utc() {
        let paris = resolve_zone("Europe/Paris").expect("a known zone");
        let schedule = Schedule::parse("0 9 * * *", "Europe/Paris").expect("parses");
        // 2026-09-17T10:00Z is 12:00 in Paris, so the next 09:00 local is the
        // following morning: 07:00Z in summer time.
        let next = schedule
            .next_after(utc_ms(2026, 9, 17, 10, 0))
            .expect("a next occurrence");
        assert_eq!(next, local_ms(paris, 2026, 9, 18, 9, 0));
        assert_eq!(next, utc_ms(2026, 9, 18, 7, 0));
    }

    #[test]
    fn the_instant_asked_about_is_not_its_own_next_occurrence() {
        let schedule = Schedule::parse("0 9 * * *", "UTC").expect("parses");
        assert_eq!(
            schedule.next_after(utc_ms(2026, 9, 17, 9, 0)),
            Some(utc_ms(2026, 9, 18, 9, 0))
        );
        assert_eq!(
            schedule.next_after(utc_ms(2026, 9, 17, 9, 0) - 1),
            Some(utc_ms(2026, 9, 17, 9, 0))
        );
    }

    #[test]
    fn a_weekday_schedule_means_that_weekday_where_the_automation_lives() {
        // 2026-09-19 is a Saturday. In Tokyo the next Monday 08:00 local is
        // 2026-09-20T23:00Z, a Sunday in UTC — the schedule is not read in UTC.
        let schedule = Schedule::parse("0 8 * * 1", "Asia/Tokyo").expect("parses");
        let next = schedule
            .next_after(utc_ms(2026, 9, 19, 12, 0))
            .expect("a next occurrence");
        assert_eq!(next, utc_ms(2026, 9, 20, 23, 0));
    }

    #[test]
    fn a_local_time_the_spring_forward_skipped_moves_to_the_next_instant_that_exists() {
        // Europe/Paris jumps from 02:00 to 03:00 on 2026-03-29, so 02:30 does
        // not exist that day. The occurrence moves forward instead of the day
        // being skipped, and the next one is the following morning.
        let paris = resolve_zone("Europe/Paris").expect("a known zone");
        let schedule = Schedule::parse("30 2 * * *", "Europe/Paris").expect("parses");
        let next = schedule
            .next_after(utc_ms(2026, 3, 28, 12, 0))
            .expect("a next occurrence");
        assert_eq!(next, utc_ms(2026, 3, 29, 1, 0));
        assert_eq!(
            next,
            local_ms(paris, 2026, 3, 29, 3, 0),
            "the instant the gap ends is 03:00 local"
        );
        assert_eq!(
            schedule.next_after(next).expect("a next occurrence"),
            utc_ms(2026, 3, 30, 0, 30)
        );
    }

    #[test]
    fn a_local_time_the_clock_repeated_fires_once_on_the_earlier_pass() {
        // Europe/Paris falls back from 03:00 to 02:00 on 2026-10-25, so 02:30
        // happens twice — 00:30Z and 01:30Z. The schedule fires once, on the
        // first pass, and the next search moves to the following day rather
        // than firing again an hour later.
        let schedule = Schedule::parse("30 2 * * *", "Europe/Paris").expect("parses");
        let first = schedule
            .next_after(utc_ms(2026, 10, 24, 12, 0))
            .expect("a next occurrence");
        assert_eq!(first, utc_ms(2026, 10, 25, 0, 30));
        assert_eq!(
            schedule.next_after(first).expect("a next occurrence"),
            utc_ms(2026, 10, 26, 1, 30),
            "the repeated hour is one occurrence, not two"
        );
    }

    #[test]
    fn a_daily_schedule_across_a_transition_keeps_its_local_time() {
        // The point of reading in a zone: 09:00 local is 09:00 local on both
        // sides of a transition, which is a different UTC instant either side.
        let schedule = Schedule::parse("0 9 * * *", "Europe/Paris").expect("parses");
        let before = schedule
            .next_after(utc_ms(2026, 10, 23, 12, 0))
            .expect("a next occurrence");
        let after = schedule
            .next_after(utc_ms(2026, 10, 26, 12, 0))
            .expect("a next occurrence");
        assert_eq!(before, utc_ms(2026, 10, 24, 7, 0), "summer time");
        assert_eq!(after, utc_ms(2026, 10, 27, 8, 0), "winter time");
    }

    #[test]
    fn a_schedule_that_can_never_match_has_no_next_occurrence() {
        let schedule = Schedule::parse("0 0 30 2 *", "UTC").expect("parses");
        assert_eq!(schedule.next_after(utc_ms(2026, 1, 1, 0, 0)), None);
    }

    #[test]
    fn six_fields_are_not_a_five_field_expression() {
        assert!(matches!(
            Schedule::parse("0 0 9 * * *", "UTC"),
            Err(ScheduleError::FieldCount(6))
        ));
        assert!(matches!(
            Schedule::parse("0 9 * *", "UTC"),
            Err(ScheduleError::FieldCount(4))
        ));
    }

    #[test]
    fn an_expression_that_does_not_parse_is_reported_as_such() {
        for expression in [
            "60 * * * *",
            "* 24 * * *",
            "* * 0 * *",
            "* * * 13 *",
            "",
            "* * * *",
        ] {
            assert!(
                matches!(
                    Schedule::parse(expression, "UTC"),
                    Err(ScheduleError::FieldCount(_)) | Err(ScheduleError::Expression(_))
                ),
                "{expression:?} should be rejected"
            );
        }
    }

    #[test]
    fn a_zone_that_does_not_exist_is_rejected_rather_than_accepted_as_syntax() {
        assert!(matches!(
            Schedule::parse("0 9 * * *", "Europe Paris"),
            Err(ScheduleError::UnknownZone(_))
        ));
        assert!(matches!(
            Schedule::parse("0 9 * * *", "Mars/Olympus"),
            Err(ScheduleError::UnknownZone(_))
        ));
        for known in [
            "UTC",
            "Europe/Paris",
            "America/Argentina/Buenos_Aires",
            "Etc/GMT+5",
        ] {
            assert!(resolve_zone(known).is_ok(), "{known} should resolve");
        }
    }

    #[test]
    fn the_previous_occurrence_is_available_for_diagnostics() {
        let schedule = Schedule::parse("0 9 * * *", "UTC").expect("parses");
        assert_eq!(
            schedule.previous_at_or_before(utc_ms(2026, 9, 17, 12, 0)),
            Some(utc_ms(2026, 9, 17, 9, 0))
        );
        assert_eq!(
            schedule.previous_at_or_before(utc_ms(2026, 9, 17, 9, 0)),
            Some(utc_ms(2026, 9, 17, 9, 0)),
            "the instant itself counts when asking about the past"
        );
    }
}
