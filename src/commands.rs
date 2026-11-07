//! Chat commands. Only `!plenum`, `!top` and `!minutes` are the bot's (and `!members` in the
//! admin chat, `!link` in a direct chat: main.rs); everything else in the room is left alone
//! (other bots may share the room).

use chrono::{Datelike, NaiveDate, NaiveTime};

use crate::{config::Publication, schedule::MeetingKey};

#[derive(Debug, PartialEq)]
pub enum Command {
    Help,
    /// `!plenum [key]`, also `!minutes` alone.
    Overview(Option<String>),
    Top {
        key: Option<String>,
        text: String,
    },
    /// `!minutes done [key]`
    Close(Option<String>),
    /// `!minutes full|link|confidential [key] [day [time]]`: what of a meeting's minutes
    /// goes into the chat; without a day the open or the next meeting's.
    Publish {
        key: Option<String>,
        mode: Publication,
        when: Option<When>,
    },
    /// `!members`: the rooms' held removals (admin chat only).
    Members,
    /// `!members confirm <room> <digest>`: carry out a room's held removals once (admin chat
    /// only), if the plan held is the one with that digest (the notice and `!members` show
    /// it); the room by a plenum key, its ID or its alias. Without a digest (empty) the answer
    /// shows the plan held and its digest.
    ConfirmRemovals {
        room: String,
        digest: String,
    },
    /// `!top` alone (maybe with a key): the topics for the next meeting.
    Topics(Option<String>),
    /// `!top #12 withdraw|later|done|again|decide|talk` (maybe with a key).
    TopicAction {
        key: Option<String>,
        number: u32,
        action: TopicAction,
    },
    /// `!plenum settings [key]`
    Settings(Option<String>),
    /// `!plenum set [key] <name> <value>`
    Set {
        key: Option<String>,
        name: String,
        value: String,
    },
    /// `!plenum reset [key] <name>`
    Reset {
        key: Option<String>,
        name: String,
    },
    /// A command of the bot with a mistake in it; the text says what is wrong.
    Invalid(String),
}

/// What `!top #12 <action>` does to topic #12.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TopicAction {
    Withdraw,
    Later,
    Done,
    Again,
    Decide,
    Talk,
}

impl TopicAction {
    fn from_word(w: &str) -> Option<Self> {
        Some(match w.to_lowercase().as_str() {
            "withdraw" | "zurückziehen" | "zurueckziehen" => Self::Withdraw,
            "later" | "später" | "spaeter" => Self::Later,
            "done" | "erledigt" => Self::Done,
            "again" | "nochmal" => Self::Again,
            "decide" | "entscheiden" => Self::Decide,
            "talk" | "besprechen" => Self::Talk,
            _ => return None,
        })
    }
}

/// `#12` (or `12`) as a topic number.
fn topic_number(w: &str) -> Option<u32> {
    let d = w.strip_prefix('#').unwrap_or(w);
    (!d.is_empty() && d.len() <= 6 && d.chars().all(|c| c.is_ascii_digit()))
        .then(|| d.parse().ok())
        .flatten()
}

/// A meeting by its day (the year may be left out: the next such day) and maybe its time.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct When {
    pub day: u32,
    pub month: u32,
    pub year: Option<i32>,
    pub time: Option<NaiveTime>,
}

impl When {
    pub fn matches(&self, start: MeetingKey) -> bool {
        let date = start.date();
        date.day() == self.day
            && date.month() == self.month
            && self.year.is_none_or(|y| y == date.year())
            && self.time.is_none_or(|t| t == start.0.time())
    }

    /// "14 Nov", "14 Nov 2026 19:00"
    pub fn describe(&self) -> String {
        let month = NaiveDate::from_ymd_opt(2000, self.month, 1)
            .map(|d| d.format("%b").to_string())
            .unwrap_or_default();
        let mut s = format!("{} {month}", self.day);
        if let Some(y) = self.year {
            s.push_str(&format!(" {y}"));
        }
        if let Some(t) = self.time {
            s.push_str(&t.format(" %H:%M").to_string());
        }
        s
    }
}

/// `14.11.`, `14.11`, `14.11.2026`, `14.11.26` or `2026-11-14`, maybe followed by `19:00`.
fn parse_when(words: &[String]) -> Result<Option<When>, String> {
    let wrong = |w: &str| {
        format!("I don't understand “{w}”. A day goes like this: `14.11.` or `14.11. 19:00`.")
    };
    let Some(first) = words.first() else {
        return Ok(None);
    };
    if words.len() > 2 {
        return Err(wrong(&words[2..].join(" ")));
    }
    let (day, month, year) = if let Ok(d) = NaiveDate::parse_from_str(first, "%Y-%m-%d") {
        (d.day(), d.month(), Some(d.year()))
    } else {
        let parts: Vec<&str> = first.trim_end_matches('.').split('.').collect();
        let num = |s: &str| s.parse::<u32>().ok();
        match parts.as_slice() {
            [d, m] => match (num(d), num(m)) {
                (Some(d), Some(m)) => (d, m, None),
                _ => return Err(wrong(first)),
            },
            [d, m, y] => {
                let y = match y.len() {
                    2 => y.parse::<i32>().ok().map(|y| 2000 + y),
                    4 => y.parse::<i32>().ok(),
                    _ => None,
                };
                match (num(d), num(m), y) {
                    (Some(d), Some(m), Some(y)) => (d, m, Some(y)),
                    _ => return Err(wrong(first)),
                }
            }
            _ => return Err(wrong(first)),
        }
    };
    // 29 Feb is a day in a leap year
    if NaiveDate::from_ymd_opt(year.unwrap_or(2000), month, day).is_none() {
        return Err(wrong(first));
    }
    let time = match words.get(1) {
        None => None,
        Some(t) => Some(NaiveTime::parse_from_str(t, "%H:%M").map_err(|_| wrong(t))?),
    };
    Ok(Some(When {
        day,
        month,
        year,
        time,
    }))
}

/// [`parse_for`] without plenum keys (every word is text).
#[cfg(test)]
pub fn parse(body: &str) -> Option<Command> {
    parse_for(body, &[])
}

/// The command in a message, if it is one of the bot's. `keys`: the plenum keys of the room,
/// recognised where a command takes one.
pub fn parse_for(body: &str, keys: &[&str]) -> Option<Command> {
    let body = body.trim();
    let (head, rest) = match body.split_once(char::is_whitespace) {
        Some((h, r)) => (h, r.trim()),
        None => (body, ""),
    };
    let head = head.to_lowercase();
    let words: Vec<&str> = rest.split_whitespace().collect();
    let is_key = |w: &str| keys.contains(&w);
    // A key anywhere among `words`, and the other words.
    let split_key = |words: &[&str]| -> (Option<String>, Vec<String>) {
        let key = words.iter().find(|w| is_key(w)).map(|w| (*w).to_owned());
        let rest = words
            .iter()
            .filter(|w| Some(**w) != key.as_deref())
            .map(|w| (*w).to_owned())
            .collect();
        (key, rest)
    };
    match head.as_str() {
        "!top" => {
            let (key, text) = match words.first() {
                Some(w) if is_key(w) => (Some((*w).to_owned()), rest[w.len()..].trim()),
                _ => (None, rest),
            };
            if text.is_empty() {
                return Some(Command::Topics(key));
            }
            // `!top #12 later`: exactly a number and an action (a topic may start with "#3 …")
            let tw: Vec<&str> = text.split_whitespace().collect();
            if let [n, w] = tw.as_slice() {
                // a phone's autocorrect adds a dot or a capital
                let w = w.trim_end_matches(['.', '!', ',', ';', '?']);
                if let (Some(number), Some(action)) = (topic_number(n), TopicAction::from_word(w)) {
                    return Some(Command::TopicAction {
                        key,
                        number,
                        action,
                    });
                }
            }
            Some(Command::Top {
                key,
                text: text.to_owned(),
            })
        }
        "!minutes" => {
            let (key, rest) = split_key(&words);
            match rest.first().map(|w| w.to_lowercase()).as_deref() {
                None => Some(Command::Overview(key)),
                Some("done") | Some("close") => Some(Command::Close(key)),
                Some(w) => match Publication::from_word(w) {
                    Some(mode) => match parse_when(&rest[1..]) {
                        Ok(when) => Some(Command::Publish { key, mode, when }),
                        Err(why) => Some(Command::Invalid(why)),
                    },
                    None => Some(Command::Invalid(
                        "Did you mean `!minutes done`, or `!minutes full`, `!minutes link` or \
                         `!minutes confidential` (what of the minutes comes into the chat)?"
                            .to_owned(),
                    )),
                },
            }
        }
        "!plenum" => {
            let sub = words.first().map(|w| w.to_lowercase());
            match sub.as_deref() {
                None => Some(Command::Overview(None)),
                Some("help") => Some(Command::Help),
                Some("settings") => {
                    let (key, rest) = split_key(&words[1..]);
                    if rest.is_empty() {
                        Some(Command::Settings(key))
                    } else {
                        Some(Command::Invalid(format!(
                            "To change one: `!plenum set {}`. `!plenum settings` alone lists them.",
                            rest.join(" ")
                        )))
                    }
                }
                Some("set") => {
                    let (key, rest) = split_key(&words[1..]);
                    match rest.as_slice() {
                        [name, value @ ..] if !value.is_empty() => Some(Command::Set {
                            key,
                            name: name.to_lowercase(),
                            value: value.join(" "),
                        }),
                        _ => Some(Command::Invalid(
                            "Like this: `!plenum set <name> <value>`, e.g. `!plenum set agenda_days 2`. \
                             `!plenum settings` shows them all."
                                .to_owned(),
                        )),
                    }
                }
                Some("reset") => {
                    let (key, rest) = split_key(&words[1..]);
                    match rest.as_slice() {
                        [name] => Some(Command::Reset {
                            key,
                            name: name.to_lowercase(),
                        }),
                        _ => Some(Command::Invalid(
                            "Like this: `!plenum reset <name>`. `!plenum settings` shows them all."
                                .to_owned(),
                        )),
                    }
                }
                // The dates are the calendar's: moved, cancelled and added there.
                Some("cancel") | Some("add") | Some("move") => Some(Command::Invalid(
                    "The meetings are in the plenum's Nextcloud calendar: add, move or cancel \
                     them there. The bot follows the calendar."
                        .to_owned(),
                )),
                Some(w) if is_key(w) && words.len() == 1 => {
                    Some(Command::Overview(Some(w.to_owned())))
                }
                Some(_) => Some(Command::Invalid(
                    "I don't know that one. `!plenum help` shows what works.".to_owned(),
                )),
            }
        }
        "!members" => match words.as_slice() {
            [] => Some(Command::Members),
            [confirm, room] if confirm.eq_ignore_ascii_case("confirm") => {
                Some(Command::ConfirmRemovals {
                    room: (*room).to_owned(),
                    digest: String::new(),
                })
            }
            [confirm, room, digest] if confirm.eq_ignore_ascii_case("confirm") => {
                Some(Command::ConfirmRemovals {
                    room: (*room).to_owned(),
                    digest: (*digest).to_owned(),
                })
            }
            _ => Some(Command::Invalid(
                "`!members` shows the held removals, `!members confirm <room> <digest>` carries \
                 out the plan with that digest (the room by its plenum key, ID or alias)."
                    .to_owned(),
            )),
        },
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> Option<Command> {
        parse_for(s, &["kitchen-floor-1", "kitchen-floor-3"])
    }

    #[test]
    fn other_messages_are_not_ours() {
        assert_eq!(p("hello"), None);
        assert_eq!(p("!help"), None);
        assert_eq!(p("!topf"), None);
        assert_eq!(p("!admin status"), None);
        // the German commands of before 2026-10-22 are gone
        assert_eq!(p("!protokoll fertig"), None);
    }

    #[test]
    fn agenda_items_with_and_without_a_key() {
        assert_eq!(
            p("!top Bins and recycling"),
            Some(Command::Top {
                key: None,
                text: "Bins and recycling".into()
            })
        );
        assert_eq!(
            p("!TOP kitchen-floor-3 dishwasher broken"),
            Some(Command::Top {
                key: Some("kitchen-floor-3".into()),
                text: "dishwasher broken".into()
            })
        );
        assert_eq!(p("!top"), Some(Command::Topics(None)));
        assert_eq!(
            p("!top kitchen-floor-1"),
            Some(Command::Topics(Some("kitchen-floor-1".into())))
        );
    }

    #[test]
    fn topic_actions_and_settings() {
        assert_eq!(
            p("!top #12 later"),
            Some(Command::TopicAction {
                key: None,
                number: 12,
                action: TopicAction::Later
            })
        );
        assert_eq!(
            p("!top kitchen-floor-1 #3 Erledigt"),
            Some(Command::TopicAction {
                key: Some("kitchen-floor-1".into()),
                number: 3,
                action: TopicAction::Done
            })
        );
        // a topic that starts with a number is a topic
        assert_eq!(
            p("!top #3 floor bathroom"),
            Some(Command::Top {
                key: None,
                text: "#3 floor bathroom".into()
            })
        );
        assert!(matches!(p("!top #x done"), Some(Command::Top { .. })));
        // as a phone writes it
        assert_eq!(
            p("!top 12 Later."),
            Some(Command::TopicAction {
                key: None,
                number: 12,
                action: TopicAction::Later
            })
        );
        assert!(matches!(
            p("!top #12 withdraw please"),
            Some(Command::Top { .. })
        ));
        assert!(matches!(
            p("!plenum settings agenda_days 2"),
            Some(Command::Invalid(t)) if t.contains("!plenum set agenda_days 2")
        ));
        assert_eq!(p("!plenum settings"), Some(Command::Settings(None)));
        assert_eq!(
            p("!plenum set kitchen-floor-3 agenda_days 2"),
            Some(Command::Set {
                key: Some("kitchen-floor-3".into()),
                name: "agenda_days".into(),
                value: "2".into()
            })
        );
        assert_eq!(
            p("!plenum reset Reminder"),
            Some(Command::Reset {
                key: None,
                name: "reminder".into()
            })
        );
        assert!(matches!(
            p("!plenum set agenda_days"),
            Some(Command::Invalid(_))
        ));
    }

    #[test]
    fn closing_and_the_overview() {
        assert_eq!(p("!minutes done"), Some(Command::Close(None)));
        assert_eq!(p("!Minutes close"), Some(Command::Close(None)));
        assert_eq!(p("!minutes"), Some(Command::Overview(None)));
        assert_eq!(
            p("!minutes done kitchen-floor-1"),
            Some(Command::Close(Some("kitchen-floor-1".into())))
        );
        assert_eq!(p("!plenum"), Some(Command::Overview(None)));
        assert_eq!(
            p("!plenum kitchen-floor-3"),
            Some(Command::Overview(Some("kitchen-floor-3".into())))
        );
        assert_eq!(p("!plenum help"), Some(Command::Help));
        assert!(matches!(p("!minutes now"), Some(Command::Invalid(_))));
        assert!(matches!(p("!plenum xyz"), Some(Command::Invalid(_))));
    }

    #[test]
    fn what_comes_into_the_chat_for_a_meeting() {
        let publish = |mode, when| {
            Some(Command::Publish {
                key: None,
                mode,
                when,
            })
        };
        assert_eq!(
            p("!minutes confidential"),
            publish(Publication::Confidential, None)
        );
        assert_eq!(p("!Minutes LINK"), publish(Publication::Link, None));
        assert_eq!(p("!minutes full"), publish(Publication::Full, None));
        assert_eq!(p("!minutes summary"), publish(Publication::Full, None));
        let nov14 = When {
            day: 14,
            month: 11,
            year: None,
            time: None,
        };
        for day in ["14.11.", "14.11"] {
            assert_eq!(
                p(&format!("!minutes confidential {day}")),
                publish(Publication::Confidential, Some(nov14))
            );
        }
        let at_seven = When {
            year: Some(2026),
            time: NaiveTime::from_hms_opt(19, 0, 0),
            ..nov14
        };
        for day in ["14.11.2026 19:00", "14.11.26 19:00", "2026-11-14 19:00"] {
            assert_eq!(
                p(&format!("!minutes link {day}")),
                publish(Publication::Link, Some(at_seven))
            );
        }
        assert_eq!(
            p("!minutes confidential kitchen-floor-1 14.11."),
            Some(Command::Publish {
                key: Some("kitchen-floor-1".into()),
                mode: Publication::Confidential,
                when: Some(nov14),
            })
        );
        assert_eq!(
            p("!minutes kitchen-floor-3 link"),
            Some(Command::Publish {
                key: Some("kitchen-floor-3".into()),
                mode: Publication::Link,
                when: None,
            })
        );
        for wrong in [
            "!minutes confidential 31.11.",
            "!minutes confidential tomorrow",
            "!minutes confidential 14.11. 7pm",
            "!minutes confidential 14.11. 19:00 please",
            "!minutes secret",
        ] {
            assert!(matches!(p(wrong), Some(Command::Invalid(_))), "{wrong}");
        }
        assert!(at_seven.matches("2026-11-14 19:00".parse().unwrap()));
        assert!(!at_seven.matches("2026-11-14 10:00".parse().unwrap()));
        assert!(nov14.matches("2027-11-14 10:00".parse().unwrap()));
        assert_eq!(at_seven.describe(), "14 Nov 2026 19:00");
    }

    #[test]
    fn held_removals_in_the_admin_chat() {
        assert_eq!(p("!members"), Some(Command::Members));
        assert_eq!(
            p("!Members CONFIRM kitchen-floor-1 1a2b3c4d"),
            Some(Command::ConfirmRemovals {
                room: "kitchen-floor-1".into(),
                digest: "1a2b3c4d".into()
            })
        );
        assert_eq!(
            p("!members confirm #kitchen-floor-1:house.org 1a2b3c4d"),
            Some(Command::ConfirmRemovals {
                room: "#kitchen-floor-1:house.org".into(),
                digest: "1a2b3c4d".into()
            })
        );
        // without the digest: the answer shows the plan held and its digest
        assert_eq!(
            p("!members confirm house"),
            Some(Command::ConfirmRemovals {
                room: "house".into(),
                digest: String::new()
            })
        );
        for wrong in [
            "!members confirm",
            "!members yes k1",
            "!members confirm a b c",
        ] {
            assert!(matches!(p(wrong), Some(Command::Invalid(_))), "{wrong}");
        }
        assert_eq!(p("!link anna"), None);
    }

    #[test]
    fn date_commands_point_to_the_calendar() {
        for body in [
            "!plenum cancel 27.10.",
            "!plenum add 30.10. 19:00",
            "!plenum move 1.11.",
        ] {
            match p(body) {
                Some(Command::Invalid(why)) => assert!(why.contains("Nextcloud calendar"), "{why}"),
                other => panic!("{body}: {other:?}"),
            }
        }
    }
}
