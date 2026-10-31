//! The end of a person's stay (the directory's `stay_until`, mxbot-common
//! `directory::Person::stay_until`): an admin may set it on anyone, typically a subtenant. The
//! admins are reminded in their direct chats with the bot `[members] stay_reminder_days` before
//! it ([`Stage::Soon`]) and on the day ([`Stage::Today`]; the day after when the bot was down
//! that day), and once more when the person still lives here more than a day after it
//! ([`Stage::Overdue`]); `!members` lists the stays that end soon and the overdue ones. Nothing
//! else follows from it: nobody is locked out or removed, the directory's groups decide that.
//! A stay is over once the person is deactivated or in none of the living groups
//! (`config::living_groups`: the house plenum's group and the subtenants'; moved out to
//! former-residents, their user still active): no reminder for them, not listed.
//!
//! Each stage is told once per person and date (remembered in the state file, so a restart
//! tells nothing twice); only the stage a stay is in now is told, never one it has passed (the
//! bot down for the reminder two weeks before: the reminder on the day is the first). A changed
//! date starts again; the memory of a person without a date (removed, deactivated, gone) is
//! forgotten. "Today" is the day in the bot's time zone (`[defaults] timezone`).
//!
//! Pure: the engine reads the directory and sends ([`crate::engine`], `remind_stays`).

use std::collections::{BTreeMap, BTreeSet};

use chrono::NaiveDate;
use mxbot_common::directory::{People, Person};
use serde::{Deserialize, Serialize};

/// Where a stay is, as the admins hear of it.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum Stage {
    /// It ends within the reminder's days.
    Soon,
    /// It ends today, or ended yesterday.
    Today,
    /// It ended more than a day ago, and the person still lives here.
    Overdue,
}

impl Stage {
    pub fn word(self) -> &'static str {
        match self {
            Stage::Soon => "soon",
            Stage::Today => "today",
            Stage::Overdue => "overdue",
        }
    }
}

/// What the admins were told about one person's stay.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Told {
    /// The end date the reminders were for (another date starts again).
    pub until: NaiveDate,
    pub stages: BTreeSet<Stage>,
}

/// Per person (the directory's id): what the admins were told ([`Told`]).
pub type Memory = BTreeMap<String, Told>;

/// The stage of a stay ending on `until`, on `today`, with the reminder `days_before`; None
/// while it is further off.
pub fn stage(until: NaiveDate, today: NaiveDate, days_before: u32) -> Option<Stage> {
    match (until - today).num_days() {
        left if left > i64::from(days_before) => None,
        left if left > 0 => Some(Stage::Soon),
        0 | -1 => Some(Stage::Today),
        _ => Some(Stage::Overdue),
    }
}

/// One reminder due.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reminder<'a> {
    pub person: &'a Person,
    pub until: NaiveDate,
    pub stage: Stage,
}

impl Reminder<'_> {
    /// The notice's transaction key: per person, date, stage and the day it went out (the
    /// homeserver drops a key it has seen, so a date set back to an earlier one is told again).
    pub fn key(&self, today: NaiveDate) -> String {
        format!(
            "members-stay:{}:{}:{}:{today}",
            self.person.id,
            self.until,
            self.stage.word()
        )
    }
}

/// Whether `person` still lives here, so their stay counts: active and in one of the `living`
/// groups (`config::living_groups`; none configured: every active person, nothing tells who
/// lives here). Someone moved out (into former-residents, their user still active) does not.
fn lives_here(person: &Person, living: &BTreeSet<String>) -> bool {
    person.active && (living.is_empty() || person.groups.iter().any(|g| living.contains(g)))
}

/// The reminders due on `today`: for each person with an end date who still lives here
/// ([`lives_here`]), its current stage, unless the admins were told it for that date already.
/// Deactivated people are left out (they are gone already; plenum-bot's deactivation notice
/// covers them), and so is who moved out (the stay is over).
pub fn due<'a>(
    people: &'a People,
    living: &BTreeSet<String>,
    told: &Memory,
    today: NaiveDate,
    days_before: u32,
) -> Vec<Reminder<'a>> {
    people
        .active()
        .filter(|person| lives_here(person, living))
        .filter_map(|person| {
            let until = person.stay_until?;
            let stage = stage(until, today, days_before)?;
            let done = told
                .get(&person.id)
                .is_some_and(|t| t.until == until && t.stages.contains(&stage));
            (!done).then_some(Reminder {
                person,
                until,
                stage,
            })
        })
        .collect()
}

/// `told` in line with a read: a person who is gone, deactivated, without a date or with another
/// date than the one they were told about is forgotten (another date starts again).
pub fn tidy(told: &mut Memory, people: &People) {
    told.retain(|id, t| {
        people
            .by_id(id)
            .is_some_and(|p| p.active && p.stay_until == Some(t.until))
    });
}

/// The admins were told `reminder`.
pub fn record(told: &mut Memory, reminder: &Reminder) {
    let entry = told
        .entry(reminder.person.id.clone())
        .or_insert_with(|| Told {
            until: reminder.until,
            stages: BTreeSet::new(),
        });
    if entry.until != reminder.until {
        *entry = Told {
            until: reminder.until,
            stages: BTreeSet::new(),
        };
    }
    entry.stages.insert(reminder.stage);
}

/// "in 3 days", "today", "yesterday", "12 days ago".
pub fn when(until: NaiveDate, today: NaiveDate) -> String {
    match (until - today).num_days() {
        0 => "today".to_owned(),
        1 => "tomorrow".to_owned(),
        -1 => "yesterday".to_owned(),
        left if left > 0 => format!("in {left} days"),
        ago => format!("{} days ago", -ago),
    }
}

/// The person as the admins' notices name them: the login, with the shown name when it differs.
fn who(person: &Person) -> String {
    let name = person.name.trim();
    if name.is_empty() || name == person.username {
        person.username.clone()
    } else {
        format!("{} ({name})", person.username)
    }
}

/// The groups they are in, for the notice ("subtenants, kitchen-floor-3").
fn groups(person: &Person) -> String {
    if person.groups.is_empty() {
        "no group".to_owned()
    } else {
        person.groups.iter().cloned().collect::<Vec<_>>().join(", ")
    }
}

/// The admins' notice (English, as the bot's other notices to them).
pub fn notice(reminder: &Reminder, today: NaiveDate) -> String {
    let person = reminder.person;
    let date = reminder.until.format("%-d %b %Y");
    let after = "Nothing happens by itself: when they move out, take them out of their groups \
                 in the directory (or put them into former-residents), or change `stay_until` \
                 there if the date is wrong.";
    match reminder.stage {
        Stage::Soon | Stage::Today => format!(
            "📅 **Members:** the stay of {who} {ends} {when} ({date}; the directory's \
             `stay_until`). They are in {groups}. {after}",
            who = who(person),
            ends = if reminder.until < today {
                "ended"
            } else {
                "ends"
            },
            when = when(reminder.until, today),
            groups = groups(person),
        ),
        Stage::Overdue => format!(
            "⏰ **Members:** the stay of {who} ended on {date} ({ago}), and they are still \
             active in the directory, in {groups}. {after}",
            who = who(person),
            ago = when(reminder.until, today),
            groups = groups(person),
        ),
    }
}

/// The `!members` lines: everyone who still lives here ([`lives_here`]) whose stay ends within
/// `days_before` or has ended, soonest first.
pub fn overview(
    people: &People,
    living: &BTreeSet<String>,
    today: NaiveDate,
    days_before: u32,
) -> Vec<String> {
    let mut stays: Vec<(NaiveDate, String)> = people
        .active()
        .filter(|p| lives_here(p, living))
        .filter_map(|p| {
            let until = p.stay_until?;
            let stage = stage(until, today, days_before)?;
            let state = match stage {
                Stage::Overdue => format!("ended {}, overdue: still active", when(until, today)),
                Stage::Today if until < today => "ended yesterday".to_owned(),
                _ => format!("ends {}", when(until, today)),
            };
            Some((
                until,
                format!(
                    "- {}: until {} ({state}; in {})",
                    who(p),
                    until.format("%-d %b %Y"),
                    groups(p)
                ),
            ))
        })
        .collect();
    stays.sort();
    stays.into_iter().map(|(_, line)| line).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use mxbot_common::directory::Choice;

    fn day(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }

    fn person(name: &str, until: Option<NaiveDate>, active: bool) -> Person {
        Person {
            id: format!("u-{name}"),
            username: name.into(),
            name: String::new(),
            active,
            accounts: Vec::new(),
            groups: if active {
                ["subtenants".to_owned(), "kitchen-floor-3".to_owned()].into()
            } else {
                BTreeSet::new()
            },
            house: None,
            links: Vec::new(),
            choice: Choice::None,
            stay_until: until,
        }
    }

    /// The house plenum's group and the subtenants'.
    fn living() -> BTreeSet<String> {
        ["residents".to_owned(), "subtenants".to_owned()].into()
    }

    /// The reminders due on `today` (14 days before), recorded as told.
    fn round(people: &People, told: &mut Memory, today: NaiveDate) -> Vec<(String, Stage)> {
        tidy(told, people);
        let due = due(people, &living(), told, today, 14);
        for r in &due {
            record(told, r);
        }
        due.iter()
            .map(|r| (r.person.username.clone(), r.stage))
            .collect()
    }

    #[test]
    fn the_stages_around_the_date() {
        let until = day(2026, 11, 30);
        assert_eq!(stage(until, day(2026, 11, 15), 14), None);
        assert_eq!(stage(until, day(2026, 11, 16), 14), Some(Stage::Soon));
        assert_eq!(stage(until, day(2026, 11, 29), 14), Some(Stage::Soon));
        assert_eq!(stage(until, day(2026, 11, 30), 14), Some(Stage::Today));
        assert_eq!(stage(until, day(2026, 12, 1), 14), Some(Stage::Today));
        assert_eq!(stage(until, day(2026, 12, 2), 14), Some(Stage::Overdue));
        assert_eq!(stage(until, day(2027, 6, 1), 14), Some(Stage::Overdue));
        // a reminder a day before
        assert_eq!(stage(until, day(2026, 11, 28), 1), None);
        assert_eq!(stage(until, day(2026, 11, 29), 1), Some(Stage::Soon));
    }

    #[test]
    fn fourteen_days_before_on_the_day_and_overdue_once_each() {
        let people = People::new(vec![person("sub", Some(day(2026, 11, 30)), true)]);
        let mut told = Memory::new();
        assert!(round(&people, &mut told, day(2026, 11, 1)).is_empty());
        assert_eq!(
            round(&people, &mut told, day(2026, 11, 16)),
            [("sub".to_owned(), Stage::Soon)]
        );
        // every round after: nothing until the day
        for d in 16..=29 {
            assert!(round(&people, &mut told, day(2026, 11, d)).is_empty());
        }
        assert_eq!(
            round(&people, &mut told, day(2026, 11, 30)),
            [("sub".to_owned(), Stage::Today)]
        );
        assert!(round(&people, &mut told, day(2026, 12, 1)).is_empty());
        assert_eq!(
            round(&people, &mut told, day(2026, 12, 2)),
            [("sub".to_owned(), Stage::Overdue)]
        );
        for d in 3..=31 {
            assert!(round(&people, &mut told, day(2026, 12, d)).is_empty());
        }
    }

    #[test]
    fn what_was_told_survives_a_restart() {
        let people = People::new(vec![person("sub", Some(day(2026, 11, 30)), true)]);
        let mut told = Memory::new();
        round(&people, &mut told, day(2026, 11, 20));
        // as the state file keeps it
        let json = serde_json::to_string(&told).unwrap();
        assert!(
            json.contains("\"2026-11-30\"") && json.contains("\"soon\""),
            "{json}"
        );
        let mut again: Memory = serde_json::from_str(&json).unwrap();
        assert!(round(&people, &mut again, day(2026, 11, 21)).is_empty());
        assert_eq!(
            round(&people, &mut again, day(2026, 11, 30)),
            [("sub".to_owned(), Stage::Today)]
        );
    }

    #[test]
    fn a_changed_date_starts_again_and_a_removed_one_is_forgotten() {
        let mut told = Memory::new();
        let at = |until| People::new(vec![person("sub", until, true)]);
        assert_eq!(
            round(&at(Some(day(2026, 11, 30))), &mut told, day(2026, 11, 20)),
            [("sub".to_owned(), Stage::Soon)]
        );
        // extended by a month: quiet until two weeks before the new date
        assert!(round(&at(Some(day(2026, 12, 31))), &mut told, day(2026, 11, 21)).is_empty());
        assert!(told.is_empty(), "the old date is forgotten");
        assert_eq!(
            round(&at(Some(day(2026, 12, 31))), &mut told, day(2026, 12, 17)),
            [("sub".to_owned(), Stage::Soon)]
        );
        // moved closer, into the window: told again for the new date
        assert_eq!(
            round(&at(Some(day(2026, 12, 20))), &mut told, day(2026, 12, 18)),
            [("sub".to_owned(), Stage::Soon)]
        );
        // the attribute removed: nothing, and nothing remembered
        assert!(round(&at(None), &mut told, day(2026, 12, 19)).is_empty());
        assert!(told.is_empty());
        // set again to the same date: told again
        assert_eq!(
            round(&at(Some(day(2026, 12, 20))), &mut told, day(2026, 12, 19)),
            [("sub".to_owned(), Stage::Soon)]
        );
    }

    #[test]
    fn only_the_stage_it_is_in_is_told_and_a_date_long_past_once_as_overdue() {
        // the bot was down two weeks before: the day itself is the first reminder
        let people = People::new(vec![
            person("late", Some(day(2026, 11, 30)), true),
            person("past", Some(day(2026, 9, 1)), true),
        ]);
        let mut told = Memory::new();
        let mut got = round(&people, &mut told, day(2026, 11, 30));
        got.sort();
        assert_eq!(
            got,
            [
                ("late".to_owned(), Stage::Today),
                ("past".to_owned(), Stage::Overdue)
            ]
        );
        assert!(round(&people, &mut told, day(2026, 11, 30)).is_empty());
        assert_eq!(
            round(&people, &mut told, day(2026, 12, 2)),
            [("late".to_owned(), Stage::Overdue)]
        );
    }

    #[test]
    fn deactivated_and_gone_people_are_ignored_and_forgotten() {
        let mut told = Memory::new();
        let active = People::new(vec![person("sub", Some(day(2026, 11, 30)), true)]);
        round(&active, &mut told, day(2026, 11, 20));
        assert_eq!(told.len(), 1);
        let deactivated = People::new(vec![person("sub", Some(day(2026, 11, 30)), false)]);
        assert!(round(&deactivated, &mut told, day(2026, 11, 30)).is_empty());
        assert!(told.is_empty());
        assert!(round(&deactivated, &mut told, day(2026, 12, 5)).is_empty());
        let gone = People::new(vec![person("other", None, true)]);
        assert!(round(&gone, &mut told, day(2026, 12, 5)).is_empty());
        assert!(overview(&deactivated, &living(), day(2026, 12, 5), 14).is_empty());
    }

    #[test]
    fn the_notices_and_the_overview() {
        let mut sub = person("sub", Some(day(2026, 11, 30)), true);
        sub.name = "Sam Sub".into();
        let people = People::new(vec![
            sub,
            person("far", Some(day(2027, 3, 1)), true),
            person("past", Some(day(2026, 10, 1)), true),
        ]);
        let today = day(2026, 11, 26);
        let due = due(&people, &living(), &Memory::new(), today, 14);
        assert_eq!(due.len(), 2);
        let soon = due.iter().find(|r| r.stage == Stage::Soon).unwrap();
        let text = notice(soon, today);
        assert!(text.contains("sub (Sam Sub)"), "{text}");
        assert!(text.contains("ends in 4 days (30 Nov 2026"), "{text}");
        assert!(text.contains("subtenants"), "{text}");
        assert!(text.contains("Nothing happens by itself"), "{text}");
        assert_eq!(
            soon.key(today),
            "members-stay:u-sub:2026-11-30:soon:2026-11-26"
        );
        let overdue = due.iter().find(|r| r.stage == Stage::Overdue).unwrap();
        let text = notice(overdue, today);
        assert!(text.contains("ended on 1 Oct 2026 (56 days ago)"), "{text}");
        let today_notice = notice(
            &Reminder {
                stage: Stage::Today,
                until: day(2026, 11, 26),
                ..soon.clone()
            },
            today,
        );
        assert!(today_notice.contains("ends today"), "{today_notice}");
        assert_eq!(
            overview(&people, &living(), today, 14),
            [
                "- past: until 1 Oct 2026 (ended 56 days ago, overdue: still active; in \
                 kitchen-floor-3, subtenants)",
                "- sub (Sam Sub): until 30 Nov 2026 (ends in 4 days; in kitchen-floor-3, \
                 subtenants)",
            ]
        );
    }

    #[test]
    fn who_moved_out_is_not_reminded_or_listed() {
        let moved = |groups: &[&str]| {
            let mut p = person("sub", Some(day(2026, 11, 30)), true);
            p.groups = groups.iter().map(|g| (*g).to_owned()).collect();
            People::new(vec![p])
        };
        let here = moved(&["subtenants", "kitchen-floor-3"]);
        // moved out properly: into former-residents (and still in the kitchen's group), the
        // user still active
        let gone = moved(&["former-residents", "kitchen-floor-3"]);
        let mut told = Memory::new();
        assert_eq!(
            round(&here, &mut told, day(2026, 11, 20)),
            [("sub".to_owned(), Stage::Soon)]
        );
        for d in [day(2026, 11, 30), day(2026, 12, 2), day(2027, 3, 1)] {
            assert!(round(&gone, &mut told, d).is_empty(), "{d}");
            assert!(overview(&gone, &living(), d, 14).is_empty(), "{d}");
        }
        assert_eq!(overview(&here, &living(), day(2026, 12, 2), 14).len(), 1);
        // a resident counts as living here too; without living groups every active person
        let resident = moved(&["residents"]);
        assert_eq!(
            due(&resident, &living(), &Memory::new(), day(2026, 12, 2), 14).len(),
            1
        );
        assert_eq!(
            due(
                &gone,
                &BTreeSet::new(),
                &Memory::new(),
                day(2026, 12, 2),
                14
            )
            .len(),
            1
        );
        // back (a new stay in the same room): reminded again
        assert_eq!(
            round(&here, &mut told, day(2026, 12, 2)),
            [("sub".to_owned(), Stage::Overdue)]
        );
    }
}
