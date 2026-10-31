//! `!link <username>`: how a person proves that a Matrix account on another server is theirs.
//! Written from that account in a direct chat with the bot, it answers with the link code
//! ([`mxbot_common::link`]) for that house user and that account, which the person enters in
//! the identity provider's flow while logged in as that user. The code says whose it is, so a
//! code someone is talked into handing over names the house user it would link to.
//!
//! Refused: house accounts (they belong to their person through the login), anything outside
//! a direct chat (the code would be read by others), more than [`PER_HOUR`] codes per account
//! and hour, and old messages (history a fresh store brings back). Without the key
//! (`MATRIX_LINK_KEY`) the bot says linking is not set up. The code is never logged.

use std::{
    collections::HashMap,
    sync::Mutex,
    time::{Duration, SystemTime},
};

use mxbot_common::{link, matrix_sdk::ruma::OwnedUserId, matrix_sdk::ruma::UserId};

/// Environment variable with the key the bot shares with the identity provider.
pub const KEY_ENV: &str = "MATRIX_LINK_KEY";

/// Codes one account gets per hour.
pub const PER_HOUR: usize = 5;

const HOUR: Duration = Duration::from_secs(3600);

pub struct LinkDesk {
    key: Option<Vec<u8>>,
    /// The house's server: its accounts need no link.
    house_server: String,
    /// The flow where the code is entered.
    choose_url: Option<String>,
    /// Codes given per account, by when (the last hour).
    given: Mutex<HashMap<OwnedUserId, Vec<SystemTime>>>,
}

impl LinkDesk {
    pub fn new(key: Option<Vec<u8>>, house_server: &str, choose_url: Option<&str>) -> Self {
        Self {
            key: key.filter(|k| !k.is_empty()),
            house_server: house_server.to_owned(),
            choose_url: choose_url.map(str::to_owned),
            given: Mutex::new(HashMap::new()),
        }
    }

    /// The key from [`KEY_ENV`] (trimmed; unset or empty: linking is off).
    pub fn from_env(house_server: &str, choose_url: Option<&str>) -> Self {
        let key = std::env::var(KEY_ENV)
            .ok()
            .map(|k| k.trim().as_bytes().to_vec());
        Self::new(key, house_server, choose_url)
    }

    pub fn enabled(&self) -> bool {
        self.key.is_some()
    }

    /// The answer to `body` from `sender`, when it is `!link`. `direct`: the room is a direct
    /// chat of the bot and `sender`; `sent`: the server's time of the message.
    pub fn answer(
        &self,
        sender: &UserId,
        body: &str,
        direct: bool,
        sent: SystemTime,
        now: SystemTime,
    ) -> Option<String> {
        let mut words = body.split_whitespace();
        if !words.next()?.eq_ignore_ascii_case("!link") {
            return None;
        }
        if now.duration_since(sent).unwrap_or_default() > crate::COMMAND_MAX_AGE {
            return None;
        }
        let args: Vec<&str> = words.collect();
        if !direct {
            return Some(
                "🔒 `!link` bitte nur in einem Direktchat mit mir: der Code ist nur für dich."
                    .to_owned(),
            );
        }
        if sender.server_name().as_str() == self.house_server {
            return Some(format!(
                "ℹ️ {sender} ist ein Hauskonto: es gehört über die Anmeldung schon zu dir und \
                 braucht keinen Code. `!link` ist für Konten auf anderen Servern (etwa \
                 matrix.org): schreib mir von dort."
            ));
        }
        let Some(key) = &self.key else {
            return Some(
                "⚠️ Das Verknüpfen von Matrix-Konten ist hier noch nicht eingerichtet. Bitte \
                 frag die Admins."
                    .to_owned(),
            );
        };
        let username = match args.as_slice() {
            [u] if is_username(u) => *u,
            [u] if u.starts_with('@') => {
                return Some(
                    "Bitte deinen Benutzernamen der Hausanmeldung, keine Matrix-ID: \
                     `!link <benutzername>`."
                        .to_owned(),
                )
            }
            _ => {
                return Some(
                    "Bitte mit deinem Benutzernamen der Hausanmeldung: `!link <benutzername>`."
                        .to_owned(),
                )
            }
        };
        if !self.take_slot(sender, now) {
            return Some(format!(
                "⏳ Du hast in der letzten Stunde schon {PER_HOUR} Codes bekommen. Bitte \
                 versuch es später noch einmal."
            ));
        }
        let code = link::code(key, username, sender, link::slot_at(now));
        let flow = match &self.choose_url {
            Some(url) => format!("unter {url}"),
            None => "im Anmeldeportal des Hauses (Matrix-Konten)".to_owned(),
        };
        Some(format!(
            "🔗 Code für das Hauskonto **{username}** und dieses Konto ({sender}):\n\n\
             **{code}**\n\n\
             Gib ihn selbst {flow} ein, angemeldet als {username}. Er gilt 30 bis 60 Minuten.\n\n\
             ⚠️ Nur selbst eingeben, an niemanden weitergeben: wer ihn hat, könnte dieses Konto \
             mit dem Hauskonto {username} verknüpfen. Bist du nicht {username}, gib ihn nirgends \
             ein."
        ))
    }

    /// Counts a code for `sender` unless they had [`PER_HOUR`] in the last hour.
    fn take_slot(&self, sender: &UserId, now: SystemTime) -> bool {
        let mut given = self.given.lock().unwrap_or_else(|e| e.into_inner());
        given.retain(|_, times| {
            times.retain(|t| now.duration_since(*t).unwrap_or_default() < HOUR);
            !times.is_empty()
        });
        let times = given.entry(sender.to_owned()).or_default();
        if times.len() >= PER_HOUR {
            return false;
        }
        times.push(now);
        true
    }
}

/// A login name as the identity provider takes it: letters, digits and `@ . + - _`, one word.
fn is_username(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 150
        && !s.starts_with('@')
        && s.chars()
            .all(|c| c.is_alphanumeric() || matches!(c, '@' | '.' | '+' | '-' | '_'))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(s: &str) -> OwnedUserId {
        UserId::parse(s).unwrap()
    }

    fn now() -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000 * link::SLOT_SECONDS + 5)
    }

    fn desk() -> LinkDesk {
        LinkDesk::new(
            Some(b"test-key".to_vec()),
            "house.org",
            Some("https://login.example.org/if/flow/house-matrix-accounts/"),
        )
    }

    fn ask(d: &LinkDesk, who: &str, body: &str) -> Option<String> {
        d.answer(&id(who), body, true, now(), now())
    }

    #[test]
    fn a_code_for_the_named_user_and_this_account() {
        let a = ask(&desk(), "@anna:matrix.org", "!link anna").unwrap();
        // the shared test vector: key test-key, anna, @anna:matrix.org, slot 1000000
        assert!(a.contains("**W710-C4F3**"), "{a}");
        assert_eq!(
            link::code(
                b"test-key",
                "anna",
                &id("@anna:matrix.org"),
                link::slot_at(now())
            ),
            "W710-C4F3"
        );
        assert!(
            a.contains("**anna**") && a.contains("@anna:matrix.org"),
            "{a}"
        );
        assert!(a.contains("https://login.example.org/if/flow/house-matrix-accounts/"));
        assert!(a.contains("Nur selbst eingeben"), "{a}");
        // the username's case does not matter
        assert!(ask(&desk(), "@anna:matrix.org", "!LINK Anna")
            .unwrap()
            .contains("W710-C4F3"));
    }

    #[test]
    fn house_accounts_other_rooms_and_old_messages_get_no_code() {
        let d = desk();
        let a = ask(&d, "@anna:house.org", "!link anna").unwrap();
        assert!(a.contains("Hauskonto") && !a.contains("W710"), "{a}");
        let a = d
            .answer(&id("@anna:matrix.org"), "!link anna", false, now(), now())
            .unwrap();
        assert!(a.contains("Direktchat") && !a.contains("W710"), "{a}");
        // history from before (a fresh store, a replay): no answer at all
        let old = now() - Duration::from_secs(16 * 60);
        assert!(d
            .answer(&id("@anna:matrix.org"), "!link anna", true, old, now())
            .is_none());
        // not ours
        assert!(ask(&d, "@anna:matrix.org", "!linked anna").is_none());
        assert!(ask(&d, "@anna:matrix.org", "hello").is_none());
    }

    #[test]
    fn a_username_is_needed_and_no_matrix_id() {
        let d = desk();
        for body in ["!link", "!link anna ben", "!link an/na"] {
            let a = ask(&d, "@anna:matrix.org", body).unwrap();
            assert!(a.contains("!link <benutzername>"), "{body}: {a}");
        }
        let a = ask(&d, "@anna:matrix.org", "!link @anna:house.org").unwrap();
        assert!(a.contains("keine Matrix-ID"), "{a}");
    }

    #[test]
    fn five_codes_an_hour_per_account() {
        let d = desk();
        for _ in 0..PER_HOUR {
            assert!(ask(&d, "@anna:matrix.org", "!link anna")
                .unwrap()
                .contains("W710"));
        }
        let a = ask(&d, "@anna:matrix.org", "!link anna").unwrap();
        assert!(a.contains("letzten Stunde") && !a.contains("W710"), "{a}");
        // another account is not held up
        assert!(ask(&d, "@ben:matrix.org", "!link ben")
            .unwrap()
            .contains("🔗"));
        // an hour later again
        let later = now() + HOUR;
        let a = d
            .answer(&id("@anna:matrix.org"), "!link anna", true, later, later)
            .unwrap();
        assert!(a.contains("🔗"), "{a}");
    }

    #[test]
    fn without_the_key_linking_is_not_set_up() {
        let d = LinkDesk::new(None, "house.org", None);
        assert!(!d.enabled());
        let a = ask(&d, "@anna:matrix.org", "!link anna").unwrap();
        assert!(a.contains("nicht eingerichtet"), "{a}");
        let d = LinkDesk::new(Some(Vec::new()), "house.org", None);
        assert!(!d.enabled());
        // without the flow's address it names the portal
        let d = LinkDesk::new(Some(b"k".to_vec()), "house.org", None);
        assert!(ask(&d, "@a:matrix.org", "!link a")
            .unwrap()
            .contains("Anmeldeportal"));
    }
}
