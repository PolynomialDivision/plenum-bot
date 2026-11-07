//! A plenum's own settings, changed in its room (`!plenum settings`, `!plenum set <name>
//! <value>`, `!plenum reset <name>`; docs/TOPICS.md, "Settings"). Only what is safe for
//! the people of a plenum to decide for themselves: when the agenda comes, the reminder, and
//! how topics behave. Who may read or do what (rooms, groups, access, publication,
//! organisers) is house-infra's.
//!
//! The values are kept raw in topics.json; each is checked again when it is used, and one that
//! no longer fits (a bound changed) counts as unset.

use std::collections::BTreeMap;

use crate::config::Plenum;

#[derive(Clone, Copy, Debug, PartialEq)]
enum Kind {
    Switch,
    /// A number from..=to, with its unit for people.
    Number(u32, u32, &'static str),
}

struct Spec {
    name: &'static str,
    kind: Kind,
    /// What it does, in plain words.
    says: &'static str,
}

const SPECS: [Spec; 5] = [
    Spec {
        name: "topics",
        kind: Kind::Switch,
        says: "numbered topics with a message each, the agenda in the announcement, outcomes in the topics' threads (off: `!top` only puts a line on the agenda)",
    },
    Spec {
        name: "agenda_days",
        kind: Kind::Number(1, 14, "days"),
        says: "how many days before a meeting its agenda and minutes come",
    },
    Spec {
        name: "reminder",
        kind: Kind::Number(0, 1440, "minutes"),
        says: "how many minutes before the start the reminder comes (0: none)",
    },
    Spec {
        name: "carry_over",
        kind: Kind::Switch,
        says: "a topic not reached goes onto the next agenda by itself",
    },
    Spec {
        name: "tell_interested",
        kind: Kind::Switch,
        says: "the outcome mentions everyone who gave the topic a 👍",
    },
];

fn spec(name: &str) -> Option<&'static Spec> {
    SPECS.iter().find(|s| s.name == name)
}

fn switch(raw: &str) -> Option<bool> {
    match raw.trim().to_lowercase().as_str() {
        "on" | "yes" | "true" | "an" | "ja" | "1" => Some(true),
        "off" | "no" | "false" | "aus" | "nein" | "0" => Some(false),
        _ => None,
    }
}

/// `value` for the setting `name`, as it is kept ("on", "3"); an error says what fits.
pub fn normalize(name: &str, value: &str) -> Result<String, String> {
    let Some(s) = spec(name) else {
        return Err(format!(
            "There is no setting “{}”. These are: {}.",
            crate::topics::md(name),
            names()
        ));
    };
    match s.kind {
        Kind::Switch => match switch(value) {
            Some(true) => Ok("on".to_owned()),
            Some(false) => Ok("off".to_owned()),
            None => Err(format!("`{}` is `on` or `off`.", s.name)),
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
fn room_value(overrides: &BTreeMap<String, String>, name: &str) -> Option<String> {
    let raw = overrides.get(name)?;
    normalize(name, raw).ok()
}

/// The value the plenum has from house-infra (the config), as people read it.
fn base_value(base: &Plenum, name: &str) -> String {
    let on = |b: bool| if b { "on" } else { "off" }.to_owned();
    match name {
        "topics" => on(base.topics.enabled),
        "agenda_days" => base.prepare_days.to_string(),
        "reminder" => base.remind_minutes.to_string(),
        "carry_over" => on(base.topics.carry_over),
        "tell_interested" => on(base.topics.tell_interested),
        _ => String::new(),
    }
}

/// The plenum with the room's values applied (what every step uses). A room cannot switch on
/// topics that house-infra switched off, and without `room_settings` it has no values.
pub fn apply(base: &Plenum, overrides: &BTreeMap<String, String>) -> Plenum {
    let mut p = base.clone();
    if !base.topics.room_settings {
        return p;
    }
    let get = |name| room_value(overrides, name);
    if let Some(v) = get("topics").and_then(|v| switch(&v)) {
        p.topics.enabled = base.topics.enabled && v;
    }
    if let Some(v) = get("agenda_days").and_then(|v| v.parse().ok()) {
        p.prepare_days = v;
    }
    if let Some(v) = get("reminder").and_then(|v| v.parse().ok()) {
        p.remind_minutes = v;
    }
    if let Some(v) = get("carry_over").and_then(|v| switch(&v)) {
        p.topics.carry_over = v;
    }
    if let Some(v) = get("tell_interested").and_then(|v| switch(&v)) {
        p.topics.tell_interested = v;
    }
    p
}

/// Why a room may not set `name` to `value`, if it may not (house-infra's limits).
pub fn refused(base: &Plenum, name: &str, value: &str) -> Option<String> {
    if !base.topics.room_settings {
        return Some(
            "This plenum's settings are set centrally by the admins, not in the room. Please ask them."
                .to_owned(),
        );
    }
    if name == "topics" && value == "on" && !base.topics.enabled {
        return Some(
            "Topics are switched off centrally for this plenum (by the admins); the room cannot switch them on."
                .to_owned(),
        );
    }
    None
}

/// The current value of `name` and where it comes from, for people.
pub fn current(base: &Plenum, overrides: &BTreeMap<String, String>, name: &str) -> String {
    match (base.topics.room_settings, room_value(overrides, name)) {
        (true, Some(v)) => v,
        _ => base_value(base, name),
    }
}

/// What `!plenum settings` shows.
pub fn describe(
    base: &Plenum,
    overrides: &BTreeMap<String, String>,
    may_change: bool,
    key_hint: &str,
) -> String {
    let mut s = format!("⚙️ **Settings of {}**\n", crate::topics::md(&base.name));
    for spec in &SPECS {
        let room = base
            .topics
            .room_settings
            .then(|| room_value(overrides, spec.name))
            .flatten();
        let (value, from) = match &room {
            Some(v) => (v.clone(), "set in this room".to_owned()),
            None => (base_value(base, spec.name), "default".to_owned()),
        };
        s.push_str(&format!(
            "\n• `{}`: **{value}** ({from}): {}",
            spec.name, spec.says
        ));
    }
    if !base.topics.room_settings {
        s.push_str("\n\nThey are set centrally by the admins for this plenum (ask them).");
    } else if may_change {
        s.push_str(&format!(
            "\n\nChange one: `!plenum set {key_hint}<name> <value>`, e.g. `!plenum set {key_hint}agenda_days 2`; \
             back to the default: `!plenum reset {key_hint}<name>`. Changes are said here."
        ));
    } else {
        s.push_str("\n\nThe plenum's organizers change them.");
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::TopicRules;

    fn base() -> Plenum {
        let mut p = crate::protocol::tests_plenum();
        p.prepare_days = 3;
        p.remind_minutes = 30;
        p.topics = TopicRules::default();
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
        assert_eq!(normalize("topics", "AUS").unwrap(), "off");
        assert_eq!(normalize("agenda_days", "2 days").unwrap(), "2");
        assert_eq!(normalize("reminder", "0").unwrap(), "0");
        assert!(normalize("agenda_days", "0").is_err());
        assert!(normalize("agenda_days", "15").is_err());
        assert!(normalize("reminder", "-5").is_err());
        assert!(normalize("carry_over", "maybe").is_err());
        let e = normalize("publication", "full").unwrap_err();
        assert!(e.contains("`agenda_days`"), "{e}");
    }

    #[test]
    fn the_room_comes_first_within_the_house_limits() {
        let b = base();
        let p = apply(
            &b,
            &set(&[
                ("agenda_days", "5"),
                ("reminder", "0"),
                ("carry_over", "off"),
            ]),
        );
        assert_eq!((p.prepare_days, p.remind_minutes), (5, 0));
        assert!(!p.topics.carry_over);
        // a value that no longer fits counts as unset
        assert_eq!(apply(&b, &set(&[("agenda_days", "99")])).prepare_days, 3);
        // topics switched off in house-infra stay off
        let mut off = base();
        off.topics.enabled = false;
        assert!(!apply(&off, &set(&[("topics", "on")])).topics.enabled);
        assert!(refused(&off, "topics", "on").is_some());
        assert!(refused(&off, "topics", "off").is_none());
        // without room settings the room's values do nothing
        let mut fixed = base();
        fixed.topics.room_settings = false;
        assert_eq!(apply(&fixed, &set(&[("agenda_days", "5")])).prepare_days, 3);
        assert!(refused(&fixed, "agenda_days", "5").is_some());
    }

    #[test]
    fn the_list_says_where_each_value_comes_from() {
        let b = base();
        let text = describe(&b, &set(&[("agenda_days", "2")]), true, "");
        assert!(
            text.contains("`agenda_days`: **2** (set in this room)"),
            "{text}"
        );
        assert!(text.contains("`reminder`: **30** (default)"), "{text}");
        assert!(text.contains("!plenum set agenda_days 2"), "{text}");
        assert_eq!(
            current(&b, &set(&[("agenda_days", "2")]), "agenda_days"),
            "2"
        );
        assert!(describe(&b, &BTreeMap::new(), false, "").contains("organizers change them"));
    }
}
