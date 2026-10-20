//! When a plenum meets: recurrence rules, one-off dates and cancellations.
//!
//! A meeting is identified by its local start ("2026-11-01 19:00", [`MeetingKey`]): the
//! rule proposes keys, `!plenum extra` adds one, `!plenum absagen` removes one. The key
//! never changes once a protocol exists for it, so a moved rule leaves past meetings alone.

use std::{collections::BTreeSet, fmt, str::FromStr};

use anyhow::{bail, Context, Result};
use chrono::{
    DateTime, Datelike, Duration, LocalResult, NaiveDate, NaiveDateTime, NaiveTime, TimeZone, Utc,
    Weekday,
};
use chrono_tz::Tz;
use mxbot_common::config::TimeOfDay;
use serde::{Deserialize, Deserializer, Serialize};

/// The local start of one meeting, written `YYYY-MM-DD HH:MM`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MeetingKey(pub NaiveDateTime);

impl MeetingKey {
    pub fn new(date: NaiveDate, time: TimeOfDay) -> Self {
        Self(date.and_time(naive_time(time)))
    }

    pub fn date(&self) -> NaiveDate {
        self.0.date()
    }

    /// The start as an instant in `tz`. A start inside a DST gap (02:30 on the spring
    /// change) moves to the first valid minute after it; an ambiguous one takes the first.
    pub fn start(&self, tz: Tz) -> DateTime<Utc> {
        let mut local = self.0;
        for _ in 0..180 {
            match tz.from_local_datetime(&local) {
                LocalResult::Single(t) | LocalResult::Ambiguous(t, _) => {
                    return t.with_timezone(&Utc)
                }
                LocalResult::None => local += Duration::minutes(1),
            }
        }
        Utc.from_utc_datetime(&self.0)
    }
}

impl fmt::Display for MeetingKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0.format("%Y-%m-%d %H:%M"))
    }
}

impl FromStr for MeetingKey {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        NaiveDateTime::parse_from_str(s.trim(), "%Y-%m-%d %H:%M")
            .map(Self)
            .with_context(|| format!("invalid meeting {s:?} (expected YYYY-MM-DD HH:MM)"))
    }
}

impl Serialize for MeetingKey {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for MeetingKey {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        String::deserialize(d)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

/// A weekday in config: `mon` … `sun` (or the English name).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Day(pub Weekday);

impl<'de> Deserialize<'de> for Day {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(d)?;
        parse_weekday(&raw)
            .map(Day)
            .ok_or_else(|| serde::de::Error::custom(format!("unknown weekday {raw:?} (mon … sun)")))
    }
}

pub fn parse_weekday(s: &str) -> Option<Weekday> {
    Some(match s.trim().to_lowercase().as_str() {
        "mon" | "monday" => Weekday::Mon,
        "tue" | "tuesday" => Weekday::Tue,
        "wed" | "wednesday" => Weekday::Wed,
        "thu" | "thursday" => Weekday::Thu,
        "fri" | "friday" => Weekday::Fri,
        "sat" | "saturday" => Weekday::Sat,
        "sun" | "sunday" => Weekday::Sun,
        _ => return None,
    })
}

/// How a plenum repeats.
#[derive(Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Recurrence {
    /// Every `every_weeks` weeks on `weekday`. With more than one week, `anchor` is a date of
    /// a meeting week (any day of that week), so the rule knows which weeks count.
    Weekly {
        weekday: Day,
        time: TimeOfDay,
        #[serde(default = "one")]
        every_weeks: u32,
        #[serde(default)]
        anchor: Option<NaiveDate>,
    },
    /// The `nths` `weekday`s of every month: `[1, 3]` = the first and third, `-1` = the last.
    MonthlyNth {
        weekday: Day,
        nths: Vec<i32>,
        time: TimeOfDay,
    },
}

fn one() -> u32 {
    1
}

impl Recurrence {
    pub fn validate(&self) -> Result<()> {
        match self {
            Recurrence::Weekly {
                every_weeks,
                anchor,
                ..
            } => {
                if *every_weeks == 0 || *every_weeks > 52 {
                    bail!("every_weeks must be 1 … 52, not {every_weeks}");
                }
                if *every_weeks > 1 && anchor.is_none() {
                    bail!("every_weeks = {every_weeks} needs an anchor (a date in a meeting week)");
                }
            }
            Recurrence::MonthlyNth { nths, .. } => {
                if nths.is_empty() {
                    bail!("nths is empty");
                }
                if let Some(bad) = nths.iter().find(|n| !matches!(**n, 1..=5 | -1)) {
                    bail!("nths: {bad} is not 1 … 5 or -1 (the last)");
                }
            }
        }
        Ok(())
    }

    /// The meetings the rule proposes from `from` to `to` (both dates included).
    pub fn between(&self, from: NaiveDate, to: NaiveDate) -> Vec<MeetingKey> {
        let mut out = Vec::new();
        let mut day = from;
        while day <= to {
            if let Some(time) = self.meets_on(day) {
                out.push(MeetingKey::new(day, time));
            }
            day = day.succ_opt().expect("date in range");
        }
        out
    }

    fn meets_on(&self, day: NaiveDate) -> Option<TimeOfDay> {
        match self {
            Recurrence::Weekly {
                weekday,
                time,
                every_weeks,
                anchor,
            } => {
                if day.weekday() != weekday.0 {
                    return None;
                }
                if *every_weeks > 1 {
                    let anchor = anchor.expect("validated");
                    let weeks = (week_start(day) - week_start(anchor))
                        .num_days()
                        .div_euclid(7);
                    if weeks.rem_euclid(i64::from(*every_weeks)) != 0 {
                        return None;
                    }
                }
                Some(*time)
            }
            Recurrence::MonthlyNth {
                weekday,
                nths,
                time,
            } => {
                if day.weekday() != weekday.0 {
                    return None;
                }
                let nth = (day.day() as i32 - 1) / 7 + 1;
                let last = day + Duration::days(7);
                let is_last = last.month() != day.month();
                nths.iter()
                    .any(|n| *n == nth || (*n == -1 && is_last))
                    .then_some(*time)
            }
        }
    }

    /// One line for `!plenum`: "jeden 1. Sonntag im Monat um 19:00".
    pub fn describe(&self) -> String {
        match self {
            Recurrence::Weekly {
                weekday,
                time,
                every_weeks,
                ..
            } => match every_weeks {
                1 => format!("jeden {} um {time}", weekday_de(weekday.0)),
                2 => format!("jeden zweiten {} um {time}", weekday_de(weekday.0)),
                n => format!("alle {n} Wochen {} um {time}", weekday_de(weekday.0)),
            },
            Recurrence::MonthlyNth {
                weekday,
                nths,
                time,
            } => {
                let mut which = nths
                    .iter()
                    .map(|n| match n {
                        -1 => "letzten".to_owned(),
                        n => format!("{n}."),
                    })
                    .collect::<Vec<_>>();
                // "1., 3. und 5."
                let last = which.pop().unwrap_or_default();
                let which = if which.is_empty() {
                    last
                } else {
                    format!("{} und {last}", which.join(", "))
                };
                format!("jeden {which} {} im Monat um {time}", weekday_de(weekday.0))
            }
        }
    }
}

fn week_start(day: NaiveDate) -> NaiveDate {
    day - Duration::days(i64::from(day.weekday().num_days_from_monday()))
}

pub fn naive_time(t: TimeOfDay) -> NaiveTime {
    NaiveTime::from_hms_opt(t.hour, t.minute, 0).expect("TimeOfDay is valid")
}

pub fn weekday_de(day: Weekday) -> &'static str {
    match day {
        Weekday::Mon => "Montag",
        Weekday::Tue => "Dienstag",
        Weekday::Wed => "Mittwoch",
        Weekday::Thu => "Donnerstag",
        Weekday::Fri => "Freitag",
        Weekday::Sat => "Samstag",
        Weekday::Sun => "Sonntag",
    }
}

/// "Sonntag, 01.11.2026 um 19:00"
pub fn describe_meeting(key: MeetingKey) -> String {
    format!(
        "{}, {} um {}",
        weekday_de(key.0.weekday()),
        key.0.format("%d.%m.%Y"),
        key.0.format("%H:%M")
    )
}

/// The meetings of a plenum from `from` to `to`: the rule's, minus the cancelled ones,
/// plus the extra ones, sorted.
pub fn planned(
    rule: Option<&Recurrence>,
    extra: &BTreeSet<MeetingKey>,
    cancelled: &BTreeSet<MeetingKey>,
    from: NaiveDate,
    to: NaiveDate,
) -> Vec<MeetingKey> {
    let mut all: BTreeSet<MeetingKey> = rule
        .map(|r| r.between(from, to))
        .unwrap_or_default()
        .into_iter()
        .collect();
    all.extend(extra.iter().filter(|k| k.date() >= from && k.date() <= to));
    all.retain(|k| !cancelled.contains(k));
    all.into_iter().collect()
}

/// A date typed in the chat: `27.10.`, `27.10.2026`, `2026-10-27`. Without a year, the
/// next such day from `today` on (today included).
pub fn parse_date(s: &str, today: NaiveDate) -> Option<NaiveDate> {
    let s = s.trim();
    if let Ok(d) = NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        return Some(d);
    }
    if let Ok(d) = NaiveDate::parse_from_str(s, "%d.%m.%Y") {
        return Some(d);
    }
    let parts: Vec<&str> = s.trim_end_matches('.').split('.').collect();
    if parts.len() == 2 {
        let day: u32 = parts[0].parse().ok()?;
        let month: u32 = parts[1].parse().ok()?;
        for year in [today.year(), today.year() + 1] {
            if let Some(d) = NaiveDate::from_ymd_opt(year, month, day) {
                if d >= today {
                    return Some(d);
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(y: i32, m: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, day).unwrap()
    }

    fn key(s: &str) -> MeetingKey {
        s.parse().unwrap()
    }

    fn rule(toml_src: &str) -> Recurrence {
        let r: Recurrence = toml::from_str(toml_src).unwrap();
        r.validate().unwrap();
        r
    }

    #[test]
    fn the_first_sunday_of_every_month() {
        let r = rule("kind = 'monthly_nth'\nweekday = 'sun'\nnths = [1]\ntime = '19:00'");
        assert_eq!(
            r.between(d(2026, 10, 1), d(2026, 12, 31)),
            vec![
                key("2026-10-04 19:00"),
                key("2026-11-01 19:00"),
                key("2026-12-06 19:00")
            ]
        );
    }

    #[test]
    fn the_first_third_and_fifth_sunday_and_the_fifth_only_when_there_is_one() {
        let r = rule("kind = 'monthly_nth'\nweekday = 'sun'\nnths = [1, 3, 5]\ntime = '19:00'");
        // November 2026 has five Sundays, December four, January 2027 five
        assert_eq!(
            r.between(d(2026, 11, 1), d(2027, 1, 31)),
            vec![
                key("2026-11-01 19:00"),
                key("2026-11-15 19:00"),
                key("2026-11-29 19:00"),
                key("2026-12-06 19:00"),
                key("2026-12-20 19:00"),
                key("2027-01-03 19:00"),
                key("2027-01-17 19:00"),
                key("2027-01-31 19:00")
            ]
        );
        assert_eq!(
            r.describe(),
            "jeden 1., 3. und 5. Sonntag im Monat um 19:00"
        );
    }

    #[test]
    fn the_last_weekday_of_a_month_is_found_in_short_and_long_months() {
        let r = rule("kind = 'monthly_nth'\nweekday = 'fri'\nnths = [-1]\ntime = '18:30'");
        assert_eq!(
            r.between(d(2026, 10, 1), d(2026, 11, 30)),
            vec![key("2026-10-30 18:30"), key("2026-11-27 18:30")]
        );
    }

    #[test]
    fn every_second_week_counts_from_the_anchor_week() {
        // 2026-10-21 is a Wednesday; the anchor may be any day of a meeting week.
        let r = rule(
            "kind = 'weekly'\nweekday = 'wed'\ntime = '20:00'\nevery_weeks = 2\nanchor = '2026-10-19'",
        );
        assert_eq!(
            r.between(d(2026, 10, 1), d(2026, 11, 20)),
            vec![
                key("2026-10-07 20:00"),
                key("2026-10-21 20:00"),
                key("2026-11-04 20:00"),
                key("2026-11-18 20:00")
            ]
        );
    }

    #[test]
    fn a_rule_that_cannot_work_is_refused() {
        for bad in [
            "kind = 'weekly'\nweekday = 'wed'\ntime = '20:00'\nevery_weeks = 2",
            "kind = 'weekly'\nweekday = 'wed'\ntime = '20:00'\nevery_weeks = 0",
            "kind = 'monthly_nth'\nweekday = 'sun'\nnths = []\ntime = '19:00'",
            "kind = 'monthly_nth'\nweekday = 'sun'\nnths = [6]\ntime = '19:00'",
        ] {
            let r: Recurrence = toml::from_str(bad).unwrap();
            assert!(r.validate().is_err(), "{bad}");
        }
        assert!(
            toml::from_str::<Recurrence>("kind = 'weekly'\nweekday = 'xx'\ntime = '20:00'")
                .is_err()
        );
        assert!(toml::from_str::<Recurrence>(
            "kind = 'weekly'\nweekday = 'wed'\ntime = '20:00'\ntypo = 1"
        )
        .is_err());
    }

    #[test]
    fn cancelled_meetings_go_and_extra_ones_come() {
        let r = rule("kind = 'monthly_nth'\nweekday = 'sun'\nnths = [1]\ntime = '19:00'");
        let extra = BTreeSet::from([key("2026-11-10 20:00"), key("2027-03-01 19:00")]);
        let cancelled = BTreeSet::from([key("2026-11-01 19:00")]);
        assert_eq!(
            planned(
                Some(&r),
                &extra,
                &cancelled,
                d(2026, 10, 20),
                d(2026, 12, 31)
            ),
            vec![key("2026-11-10 20:00"), key("2026-12-06 19:00")]
        );
        assert_eq!(
            planned(None, &extra, &cancelled, d(2026, 10, 20), d(2026, 12, 31)),
            vec![key("2026-11-10 20:00")]
        );
    }

    #[test]
    fn a_start_in_the_spring_gap_moves_to_the_first_valid_minute() {
        let tz: Tz = "Europe/Berlin".parse().unwrap();
        let start = key("2027-03-28 02:30").start(tz);
        assert_eq!(start, Utc.with_ymd_and_hms(2027, 3, 28, 1, 0, 0).unwrap());
        let winter = key("2026-11-01 19:00").start(tz);
        assert_eq!(winter, Utc.with_ymd_and_hms(2026, 11, 1, 18, 0, 0).unwrap());
    }

    #[test]
    fn dates_from_the_chat() {
        let today = d(2026, 10, 20);
        assert_eq!(parse_date("27.10.", today), Some(d(2026, 10, 27)));
        assert_eq!(parse_date("27.10", today), Some(d(2026, 10, 27)));
        assert_eq!(parse_date("3.1.", today), Some(d(2027, 1, 3)));
        assert_eq!(parse_date("20.10.", today), Some(d(2026, 10, 20)));
        assert_eq!(parse_date("27.10.2026", today), Some(d(2026, 10, 27)));
        assert_eq!(parse_date("2026-10-27", today), Some(d(2026, 10, 27)));
        assert_eq!(parse_date("31.02.", today), None);
        assert_eq!(parse_date("morgen", today), None);
    }

    #[test]
    fn rules_read_like_a_sentence() {
        let r = rule("kind = 'monthly_nth'\nweekday = 'sun'\nnths = [1, -1]\ntime = '19:00'");
        assert_eq!(
            r.describe(),
            "jeden 1. und letzten Sonntag im Monat um 19:00"
        );
        assert_eq!(
            describe_meeting(key("2026-11-01 19:00")),
            "Sonntag, 01.11.2026 um 19:00"
        );
    }
}
