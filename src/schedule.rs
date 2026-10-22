//! When a meeting is: its local start ([`MeetingKey`], "2026-11-01 19:00") and how it is
//! written in German. The dates themselves come from the plenum's Nextcloud calendar
//! (calendar.rs); a meeting is the calendar's occurrence, found again by its ID when it moves.

use std::{fmt, str::FromStr};

use anyhow::{Context, Result};
use chrono::{DateTime, Datelike, Duration, LocalResult, NaiveDateTime, TimeZone, Utc, Weekday};
use chrono_tz::Tz;
use serde::{Deserialize, Deserializer, Serialize};

/// The local start of one meeting, written `YYYY-MM-DD HH:MM`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MeetingKey(pub NaiveDateTime);

impl MeetingKey {
    pub fn date(&self) -> chrono::NaiveDate {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn key(s: &str) -> MeetingKey {
        s.parse().unwrap()
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
    fn a_meeting_reads_like_german() {
        assert_eq!(
            describe_meeting(key("2026-11-01 19:00")),
            "Sonntag, 01.11.2026 um 19:00"
        );
        assert!("2026-11-01".parse::<MeetingKey>().is_err());
    }
}
