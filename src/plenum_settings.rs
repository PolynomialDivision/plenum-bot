//! A plenum's own settings, changed in its room (`!plenum settings`, `!plenum set <name>
//! <value>`, `!plenum reset <name>`; docs/PLENUM-PAGES.md §6). Only what the people of a
//! plenum may decide for themselves: what the bot posts by itself, the reminder, when the page
//! opens. Who may read or do what (rooms, groups, access, publication, organizers) is
//! house-infra's.
//!
//! The values are kept raw in `store/settings.json` (a file of its own, so an older image
//! after a rollback neither reads nor drops it); each is checked again when it is used, and one
//! that no longer fits counts as unset.

use std::collections::BTreeMap;

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

use crate::config::{Messages, Plenum, PREPARE_DAYS_MAX, REMIND_MINUTES_MAX};

/// Changes one person may make a day, and one plenum (admins are not counted).
pub const PER_PERSON_A_DAY: usize = 10;
pub const PER_PLENUM_A_DAY: usize = 30;

/// Changes kept per plenum (for `!plenum settings` and the limits), and for how long.
const KEEP_CHANGES: usize = 50;
const KEEP_CHANGES_DAYS: i64 = 30;

/// What `store/settings.json` holds: per plenum key, its room's values and their changes.
#[derive(Serialize, Deserialize, Default, Debug, Clone, PartialEq)]
pub struct Store {
    #[serde(default)]
    pub plenums: BTreeMap<String, RoomSettings>,
}

#[derive(Serialize, Deserialize, Default, Debug, Clone, PartialEq)]
pub struct RoomSettings {
    /// By setting name, the value as `normalize` gives it.
    #[serde(default)]
    pub values: BTreeMap<String, String>,
    #[serde(default)]
    pub changes: Vec<Change>,
}

/// One change, for the list and the limits.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct Change {
    /// The name shown, and the account (for the limits).
    pub by: String,
    pub by_user: String,
    pub name: String,
    pub from: String,
    pub to: String,
    pub at: DateTime<Utc>,
}

impl RoomSettings {
    /// Whether `user` may make another change today (`admin`: always).
    pub fn may_change(&self, user: &str, admin: bool, now: DateTime<Utc>) -> bool {
        if admin {
            return true;
        }
        let today: Vec<&Change> = self
            .changes
            .iter()
            .filter(|c| now - c.at < Duration::days(1))
            .collect();
        today.len() < PER_PLENUM_A_DAY
            && today.iter().filter(|c| c.by_user == user).count() < PER_PERSON_A_DAY
    }

    /// Old changes go (the values stay).
    pub fn prune(&mut self, now: DateTime<Utc>) {
        self.changes
            .retain(|c| now - c.at < Duration::days(KEEP_CHANGES_DAYS));
        let over = self.changes.len().saturating_sub(KEEP_CHANGES);
        self.changes.drain(..over);
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Kind {
    Messages,
    /// A number from..=to, with its unit for people.
    Number(u32, u32, &'static str),
}

struct Spec {
    name: &'static str,
    kind: Kind,
    /// What it does, in plain words.
    says: &'static str,
}

const SPECS: [Spec; 3] = [
    Spec {
        name: "messages",
        kind: Kind::Messages,
        says: "`quiet`: the bot posts only the link to a meeting's new page here (a moved or cancelled meeting edits that message); `all`: also the reminder, cancellations and the summary after the plenum",
    },
    Spec {
        name: "reminder",
        kind: Kind::Number(0, REMIND_MINUTES_MAX, "minutes"),
        says: "how many minutes before the start the reminder comes (0: none; only with `messages all`)",
    },
    Spec {
        name: "page_days",
        kind: Kind::Number(1, PREPARE_DAYS_MAX, "days"),
        says: "how many days before a meeting its page opens at the earliest (it opens only once the meeting before is over)",
    },
];

fn spec(name: &str) -> Option<&'static Spec> {
    SPECS.iter().find(|s| s.name == name)
}

/// Chat text as plain text in a Matrix message (no formatting of its own).
fn md(text: &str) -> String {
    let mut out = String::new();
    for c in text.chars().filter(|c| !c.is_control()).take(60) {
        if "\\`*_[]<>#|~".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// `value` for the setting `name`, as it is kept ("quiet", "3"); an error says what fits.
pub fn normalize(name: &str, value: &str) -> Result<String, String> {
    let Some(s) = spec(name) else {
        return Err(format!(
            "There is no setting “{}”. These are: {}.",
            md(name),
            names()
        ));
    };
    match s.kind {
        Kind::Messages => match Messages::from_word(value.trim()) {
            Some(m) => Ok(m.word().to_owned()),
            None => Err("`messages` is `quiet` or `all`.".to_owned()),
        },
        Kind::Number(from, to, unit) => {
            let digits = value
                .trim()
                .trim_end_matches(|c: char| c.is_alphabetic())
                .trim();
            match digits.parse::<u32>() {
                Ok(n) if (from..=to).contains(&n) => Ok(n.to_string()),
                _ => Err(format!(
                    "`{}` is a number of {unit} from {from} to {to}, e.g. `!plenum set {} {}`.",
                    s.name,
                    s.name,
                    (from + to) / 4 + from
                )),
            }
        }
    }
}

fn names() -> String {
    SPECS
        .iter()
        .map(|s| format!("`{}`", s.name))
        .collect::<Vec<_>>()
        .join(", ")
}

pub fn is_setting(name: &str) -> bool {
    spec(name).is_some()
}

/// The room's value of `name`, if it still fits.
fn room_value(values: &BTreeMap<String, String>, name: &str) -> Option<String> {
    normalize(name, values.get(name)?).ok()
}

/// The value the plenum has from house-infra (the config), as people read it.
fn base_value(base: &Plenum, name: &str) -> String {
    match name {
        "messages" => base.messages.word().to_owned(),
        "reminder" => base.remind_minutes.to_string(),
        "page_days" => base.prepare_days.to_string(),
        _ => String::new(),
    }
}

/// The plenum with the room's values applied (what every step uses); without `room_settings`
/// the room has none.
pub fn apply(base: &Plenum, values: &BTreeMap<String, String>) -> Plenum {
    let mut p = base.clone();
    if !base.room_settings {
        return p;
    }
    let get = |name| room_value(values, name);
    if let Some(v) = get("messages").and_then(|v| Messages::from_word(&v)) {
        p.messages = v;
    }
    if let Some(v) = get("reminder").and_then(|v| v.parse().ok()) {
        p.remind_minutes = v;
    }
    if let Some(v) = get("page_days").and_then(|v| v.parse().ok()) {
        p.prepare_days = v;
    }
    p
}

/// Why the room may not change its settings, if it may not.
pub fn refused(base: &Plenum) -> Option<String> {
    (!base.room_settings).then(|| {
        "This plenum's settings are set centrally by the admins, not in the room. Please ask them."
            .to_owned()
    })
}

/// The current value of `name`, for people.
pub fn current(base: &Plenum, values: &BTreeMap<String, String>, name: &str) -> String {
    match (base.room_settings, room_value(values, name)) {
        (true, Some(v)) => v,
        _ => base_value(base, name),
    }
}

/// What `!plenum settings` shows.
pub fn describe(base: &Plenum, room: &RoomSettings, may_change: bool, key_hint: &str) -> String {
    let mut s = format!("⚙️ **Settings of {}**\n", md(&base.name));
    for spec in &SPECS {
        let own = base
            .room_settings
            .then(|| room_value(&room.values, spec.name))
            .flatten();
        let (value, from) = match &own {
            Some(v) => (v.clone(), "set in this room"),
            None => (base_value(base, spec.name), "default"),
        };
        s.push_str(&format!(
            "\n• `{}`: **{value}** ({from}): {}",
            spec.name, spec.says
        ));
    }
    if !base.room_settings {
        s.push_str("\n\nThey are set centrally by the admins for this plenum (ask them).");
    } else if may_change {
        s.push_str(&format!(
            "\n\nChange one: `!plenum set {key_hint}<name> <value>`, e.g. `!plenum set {key_hint}messages quiet`; \
             back to the default: `!plenum reset {key_hint}<name>`."
        ));
    } else {
        s.push_str("\n\nThe plenum's organizers change them.");
    }
    if let Some(c) = room.changes.last() {
        s.push_str(&format!(
            "\n\nLast change: {} set `{}` from {} to {} ({}).",
            md(&c.by),
            c.name,
            c.from,
            c.to,
            c.at.format("%-d %b")
        ));
    }
    s
}

/// The answer (and, from elsewhere, the room's notice) for a change.
pub fn change_text(plenum: &Plenum, by: &str, name: &str, from: &str, to: &str) -> String {
    format!(
        "⚙️ {} changed `{name}` of {}: {from} → **{to}**",
        md(by),
        md(&plenum.name)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> Plenum {
        let mut p = crate::protocol::tests_plenum();
        p.prepare_days = 3;
        p.remind_minutes = 30;
        p
    }

    fn set(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    #[test]
    fn values_are_checked_and_kept_plainly() {
        assert_eq!(normalize("messages", "QUIET").unwrap(), "quiet");
        assert_eq!(normalize("messages", "all").unwrap(), "all");
        assert_eq!(normalize("page_days", "28 days").unwrap(), "28");
        assert_eq!(normalize("reminder", "0").unwrap(), "0");
        assert!(normalize("page_days", "0").is_err());
        assert!(normalize("page_days", "61").is_err());
        assert!(normalize("reminder", "-5").is_err());
        assert!(normalize("messages", "some").is_err());
        let e = normalize("publication", "full").unwrap_err();
        assert!(e.contains("`messages`"), "{e}");
    }

    #[test]
    fn the_room_comes_first_unless_the_house_fixed_it() {
        let b = base();
        let p = apply(
            &b,
            &set(&[
                ("page_days", "28"),
                ("reminder", "0"),
                ("messages", "quiet"),
            ]),
        );
        assert_eq!((p.prepare_days, p.remind_minutes), (28, 0));
        assert_eq!(p.messages, Messages::Quiet);
        // a value that no longer fits counts as unset
        assert_eq!(apply(&b, &set(&[("page_days", "99")])).prepare_days, 3);
        let mut fixed = base();
        fixed.room_settings = false;
        assert_eq!(apply(&fixed, &set(&[("page_days", "5")])).prepare_days, 3);
        assert!(refused(&fixed).is_some());
        assert!(refused(&b).is_none());
    }

    #[test]
    fn the_list_says_where_each_value_comes_from() {
        let b = base();
        let room = RoomSettings {
            values: set(&[("page_days", "2")]),
            changes: Vec::new(),
        };
        let text = describe(&b, &room, true, "");
        assert!(
            text.contains("`page_days`: **2** (set in this room)"),
            "{text}"
        );
        assert!(text.contains("`reminder`: **30** (default)"), "{text}");
        assert!(text.contains("`messages`: **all** (default)"), "{text}");
        assert!(text.contains("!plenum set messages quiet"), "{text}");
        assert_eq!(current(&b, &room.values, "page_days"), "2");
        assert!(describe(&b, &room, false, "").contains("organizers change them"));
    }

    #[test]
    fn changes_are_limited_per_person_and_plenum() {
        let now = chrono::Utc::now();
        let mut room = RoomSettings::default();
        let change = |user: &str| Change {
            by: user.into(),
            by_user: user.into(),
            name: "reminder".into(),
            from: "30".into(),
            to: "60".into(),
            at: now,
        };
        for _ in 0..PER_PERSON_A_DAY {
            room.changes.push(change("@a:x"));
        }
        assert!(!room.may_change("@a:x", false, now));
        assert!(room.may_change("@a:x", true, now));
        assert!(room.may_change("@b:x", false, now));
        assert!(room.may_change("@a:x", false, now + Duration::days(2)));
        room.prune(now + Duration::days(31));
        assert!(room.changes.is_empty());
    }
}
