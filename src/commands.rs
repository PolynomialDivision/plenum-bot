//! Chat commands. Only `!plenum`, `!top` and `!protokoll` are the bot's; everything else in
//! the room is left alone (other bots share the house chat).

use chrono::NaiveDate;

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
pub fn parse_for(body: &str, _today: NaiveDate, keys: &[&str]) -> Option<Command> {
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
                    "Please add the topic: `!top <topic>`".to_owned(),
                ));
            }
            Some(Command::Top {
                key,
                text: text.to_owned(),
            })
        }
        // `!minutes done`; the German `!protokoll fertig` of before 2026-10-22 too.
        "!minutes" | "!protokoll" => {
            let (key, rest) = split_key(&words);
            match rest.first().map(|w| w.to_lowercase()).as_deref() {
                None => Some(Command::Overview(key)),
                Some("done") | Some("close") | Some("fertig") | Some("abschliessen")
                | Some("abschließen") => Some(Command::Close(key)),
                Some(_) => Some(Command::Invalid("Did you mean `!minutes done`?".to_owned())),
            }
        }
        "!plenum" => {
            let sub = words.first().map(|w| w.to_lowercase());
            match sub.as_deref() {
                None => Some(Command::Overview(None)),
                Some("hilfe") | Some("help") => Some(Command::Help),
                // Dates are the calendar's now (2026-10-22): moved, cancelled and added there.
                Some("absagen") | Some("extra") | Some("zusatz") | Some("verschieben")
                | Some("verlegen") | Some("cancel") | Some("add") | Some("move") => {
                    Some(Command::Invalid(
                        "The meetings are in the plenum's Nextcloud calendar now: add, move or \
                         cancel them there. The bot follows the calendar."
                            .to_owned(),
                    ))
                }
                Some(w) if is_key(w) && words.len() == 1 => {
                    Some(Command::Overview(Some(w.to_owned())))
                }
                Some(_) => Some(Command::Invalid(
                    "I don't know that one. `!plenum help` shows what works.".to_owned(),
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
        assert_eq!(p("!minutes done"), Some(Command::Close(None)));
        assert_eq!(p("!Minutes close"), Some(Command::Close(None)));
        assert_eq!(p("!minutes"), Some(Command::Overview(None)));
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
        assert_eq!(p("!plenum help"), Some(Command::Help));
        assert!(matches!(p("!protokoll los"), Some(Command::Invalid(_))));
        assert!(matches!(p("!plenum xyz"), Some(Command::Invalid(_))));
    }

    #[test]
    fn date_commands_point_to_the_calendar() {
        for body in [
            "!plenum absagen 27.10.",
            "!plenum extra 30.10. 19:00",
            "!plenum verschieben 1.11. 8.11.",
            "!plenum cancel 27.10.",
        ] {
            match p(body) {
                Some(Command::Invalid(why)) => assert!(why.contains("Nextcloud calendar"), "{why}"),
                other => panic!("{body}: {other:?}"),
            }
        }
    }
}
