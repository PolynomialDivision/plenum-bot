//! Chat commands. Only `!plenum`, `!top` and `!protokoll` are the bot's; everything else in
//! the room is left alone (other bots share the house chat).

use chrono::NaiveDate;
use mxbot_common::config::TimeOfDay;

use crate::schedule::parse_date;

#[derive(Debug, PartialEq)]
pub enum Command {
    Help,
    /// `!plenum [key]`, also `!protokoll` alone.
    Overview(Option<String>),
    Top {
        key: Option<String>,
        text: String,
    },
    /// `!protokoll fertig [key]`
    Close(Option<String>),
    Cancel {
        key: Option<String>,
        date: NaiveDate,
    },
    Extra {
        key: Option<String>,
        date: NaiveDate,
        time: TimeOfDay,
    },
    /// A command of the bot with a mistake in it; the text says what is wrong.
    Invalid(String),
}

/// [`parse_for`] without plenum keys (every word is text).
#[cfg(test)]
pub fn parse(body: &str, today: NaiveDate) -> Option<Command> {
    parse_for(body, today, &[])
}

/// The command in a message, if it is one of the bot's. `keys`: the plenum keys of the room,
/// recognised where a command takes one.
pub fn parse_for(body: &str, today: NaiveDate, keys: &[&str]) -> Option<Command> {
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
                return Some(Command::Invalid(
                    "Bitte das Thema dazuschreiben: `!top <Thema>`".to_owned(),
                ));
            }
            Some(Command::Top {
                key,
                text: text.to_owned(),
            })
        }
        "!protokoll" => {
            let (key, rest) = split_key(&words);
            match rest.first().map(|w| w.to_lowercase()).as_deref() {
                None => Some(Command::Overview(key)),
                Some("fertig") | Some("abschliessen") | Some("abschließen") => {
                    Some(Command::Close(key))
                }
                Some(_) => Some(Command::Invalid(
                    "Meintest du `!protokoll fertig`?".to_owned(),
                )),
            }
        }
        "!plenum" => {
            let sub = words.first().map(|w| w.to_lowercase());
            match sub.as_deref() {
                None => Some(Command::Overview(None)),
                Some("hilfe") | Some("help") => Some(Command::Help),
                Some("absagen") => {
                    let (key, rest) = split_key(&words[1..]);
                    let Some(date) = rest.first().and_then(|d| parse_date(d, today)) else {
                        return Some(Command::Invalid(
                            "Welcher Tag? Zum Beispiel `!plenum absagen 27.10.`".to_owned(),
                        ));
                    };
                    Some(Command::Cancel { key, date })
                }
                Some("extra") | Some("zusatz") => {
                    let (key, rest) = split_key(&words[1..]);
                    let date = rest.first().and_then(|d| parse_date(d, today));
                    let time = rest
                        .get(1)
                        .and_then(|t| t.trim_end_matches("Uhr").parse::<TimeOfDay>().ok());
                    match (date, time) {
                        (Some(date), Some(time)) => Some(Command::Extra { key, date, time }),
                        _ => Some(Command::Invalid(
                            "Tag und Uhrzeit, zum Beispiel `!plenum extra 30.10. 19:00`".to_owned(),
                        )),
                    }
                }
                Some(w) if is_key(w) && words.len() == 1 => {
                    Some(Command::Overview(Some(w.to_owned())))
                }
                Some(_) => Some(Command::Invalid(
                    "Das kenne ich nicht. `!plenum hilfe` zeigt, was geht.".to_owned(),
                )),
            }
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn today() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 10, 20).unwrap()
    }

    fn p(s: &str) -> Option<Command> {
        parse_for(s, today(), &["kueche-eg", "kueche-og"])
    }

    fn d(m: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, m, day).unwrap()
    }

    #[test]
    fn other_messages_are_not_ours() {
        assert_eq!(p("hallo"), None);
        assert_eq!(p("!help"), None);
        assert_eq!(p("!topf"), None);
        assert_eq!(p("!admin status"), None);
    }

    #[test]
    fn agenda_items_with_and_without_a_key() {
        assert_eq!(
            p("!top Müll und Tonnen"),
            Some(Command::Top {
                key: None,
                text: "Müll und Tonnen".into()
            })
        );
        assert_eq!(
            p("!TOP kueche-og Spülmaschine kaputt"),
            Some(Command::Top {
                key: Some("kueche-og".into()),
                text: "Spülmaschine kaputt".into()
            })
        );
        assert!(matches!(p("!top"), Some(Command::Invalid(_))));
        assert!(matches!(p("!top kueche-eg"), Some(Command::Invalid(_))));
    }

    #[test]
    fn closing_and_the_overview() {
        assert_eq!(p("!protokoll fertig"), Some(Command::Close(None)));
        assert_eq!(
            p("!protokoll fertig kueche-eg"),
            Some(Command::Close(Some("kueche-eg".into())))
        );
        assert_eq!(p("!protokoll"), Some(Command::Overview(None)));
        assert_eq!(p("!plenum"), Some(Command::Overview(None)));
        assert_eq!(
            p("!plenum kueche-og"),
            Some(Command::Overview(Some("kueche-og".into())))
        );
        assert_eq!(p("!plenum hilfe"), Some(Command::Help));
        assert!(matches!(p("!protokoll los"), Some(Command::Invalid(_))));
        assert!(matches!(p("!plenum xyz"), Some(Command::Invalid(_))));
    }

    #[test]
    fn dates_are_cancelled_and_added() {
        assert_eq!(
            p("!plenum absagen 27.10."),
            Some(Command::Cancel {
                key: None,
                date: d(10, 27)
            })
        );
        assert_eq!(
            p("!plenum absagen kueche-eg 27.10."),
            Some(Command::Cancel {
                key: Some("kueche-eg".into()),
                date: d(10, 27)
            })
        );
        assert_eq!(
            p("!plenum extra 30.10. 19:30 kueche-og"),
            Some(Command::Extra {
                key: Some("kueche-og".into()),
                date: d(10, 30),
                time: TimeOfDay::new(19, 30)
            })
        );
        assert_eq!(
            p("!plenum extra 30.10. 19:30Uhr"),
            Some(Command::Extra {
                key: None,
                date: d(10, 30),
                time: TimeOfDay::new(19, 30)
            })
        );
        assert!(matches!(
            p("!plenum absagen morgen"),
            Some(Command::Invalid(_))
        ));
        assert!(matches!(
            p("!plenum extra 30.10."),
            Some(Command::Invalid(_))
        ));
    }
}
