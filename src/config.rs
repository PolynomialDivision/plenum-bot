//! The config file: `[matrix]` and `[security]` (mxbot-common), `[outline]`, `[defaults]`,
//! `[pdf]` and one `[[plenum]]` table per plenum.

use std::collections::HashSet;

use anyhow::{bail, Context, Result};
use chrono_tz::Tz;
use mxbot_common::{
    config::{MatrixConfig, SecurityConfig},
    matrix_sdk::ruma::{OwnedRoomId, OwnedUserId},
};
use serde::Deserialize;

use crate::schedule::Recurrence;

/// Environment variable with the Outline API key (overrides `[outline] api_key`).
pub const API_KEY_ENV: &str = "OUTLINE_API_KEY";

#[derive(Deserialize)]
pub struct Config {
    pub matrix: MatrixConfig,
    #[serde(default)]
    pub security: SecurityConfig,
    pub outline: OutlineConfig,
    #[serde(default)]
    pub defaults: Defaults,
    #[serde(default)]
    pub pdf: PdfConfig,
    #[serde(rename = "plenum", default)]
    pub plenums: Vec<PlenumConfig>,
}

#[derive(Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct OutlineConfig {
    /// Where the bot reaches the API, e.g. `http://outline:3000` (no `/api`).
    pub url: String,
    /// The address people open, for the links in the chat; default `url`.
    #[serde(default)]
    pub public_url: Option<String>,
    /// Better in the environment ([`API_KEY_ENV`]).
    #[serde(default)]
    pub api_key: String,
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
}

fn default_timeout() -> u64 {
    30
}

/// Values every plenum gets unless its own table says otherwise.
#[derive(Deserialize, Clone)]
#[serde(deny_unknown_fields, default)]
pub struct Defaults {
    pub timezone: String,
    /// The protocol is created and announced this many days before the meeting.
    pub prepare_days: u32,
    /// A reminder this many minutes before the start; 0 = none.
    pub remind_minutes: u32,
    pub duration_minutes: u32,
    /// The protocol is closed (decisions collected, PDF, summary) this many hours after the
    /// end, unless someone says `!protokoll fertig` earlier.
    pub close_after_hours: u32,
}

impl Default for Defaults {
    fn default() -> Self {
        Self {
            timezone: "Europe/Berlin".to_owned(),
            prepare_days: 3,
            remind_minutes: 30,
            duration_minutes: 120,
            close_after_hours: 12,
        }
    }
}

#[derive(Deserialize, Clone)]
#[serde(deny_unknown_fields, default)]
pub struct PdfConfig {
    pub enabled: bool,
    /// DejaVuSans.ttf and DejaVuSans-Bold.ttf (Debian: fonts-dejavu-core).
    pub font_dir: String,
    /// Also send the PDF into the plenum's room (besides attaching it to the protocol).
    pub post_to_room: bool,
}

impl Default for PdfConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            font_dir: "/usr/share/fonts/truetype/dejavu".to_owned(),
            post_to_room: true,
        }
    }
}

#[derive(Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct PlenumConfig {
    /// Short name for commands and the state file: `haus`, `kueche-eg`.
    pub key: String,
    /// Shown in titles and messages: "Hausplenum".
    pub name: String,
    /// The room the plenum belongs to (announcements, `!top`, the summary).
    pub room: String,
    /// The Outline collection of its protocols (created if missing; may be shared).
    pub collection: String,
    /// None: only dates added with `!plenum extra`.
    #[serde(default)]
    pub recurrence: Option<Recurrence>,
    #[serde(default)]
    pub prepare_days: Option<u32>,
    #[serde(default)]
    pub remind_minutes: Option<u32>,
    #[serde(default)]
    pub duration_minutes: Option<u32>,
    #[serde(default)]
    pub close_after_hours: Option<u32>,
    /// Who may cancel or add dates; empty = everyone in the room (admins always).
    #[serde(default)]
    pub organizers: Vec<String>,
}

/// A plenum with every default applied and every ID parsed.
#[derive(Clone, Debug)]
pub struct Plenum {
    pub key: String,
    pub name: String,
    pub room: OwnedRoomId,
    pub collection: String,
    pub recurrence: Option<Recurrence>,
    pub prepare_days: u32,
    pub remind_minutes: u32,
    pub duration_minutes: u32,
    pub close_after_hours: u32,
    pub organizers: HashSet<OwnedUserId>,
}

impl Plenum {
    pub fn template_title(&self) -> String {
        format!("Vorlage {}", self.name)
    }

    pub fn decisions_title(&self) -> String {
        format!("Beschlüsse {}", self.name)
    }
}

impl Config {
    pub fn api_key(&self) -> Result<String> {
        let key = std::env::var(API_KEY_ENV)
            .ok()
            .filter(|v| !v.trim().is_empty())
            .unwrap_or_else(|| self.outline.api_key.clone());
        if key.trim().is_empty() {
            bail!("no Outline API key: set {API_KEY_ENV} or [outline] api_key");
        }
        Ok(key.trim().to_owned())
    }

    pub fn timezone(&self) -> Result<Tz> {
        self.defaults
            .timezone
            .parse()
            .map_err(|e| anyhow::anyhow!("[defaults] timezone {:?}: {e}", self.defaults.timezone))
    }

    /// Every plenum checked and completed with the defaults.
    pub fn plenums(&self) -> Result<Vec<Plenum>> {
        if self.plenums.is_empty() {
            bail!("no [[plenum]] in the config");
        }
        let mut keys = HashSet::new();
        let mut titles = HashSet::new();
        let mut out = Vec::new();
        for p in &self.plenums {
            let at = format!("[[plenum]] {:?}", p.key);
            if p.key.is_empty()
                || !p
                    .key
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
            {
                bail!("{at}: key must be lowercase letters, digits and '-'");
            }
            if !keys.insert(p.key.clone()) {
                bail!("{at}: key used twice");
            }
            if p.name.trim().is_empty() || p.collection.trim().is_empty() {
                bail!("{at}: name and collection must not be empty");
            }
            // Titles are how the bot finds its documents again: two plenums with one name
            // in one collection would share their template and their decisions.
            if !titles.insert((
                p.collection.trim().to_lowercase(),
                p.name.trim().to_lowercase(),
            )) {
                bail!("{at}: another plenum has the same name in the same collection");
            }
            let room = mxbot_common::rooms::parse_room_id(&format!("{at} room"), &p.room)?;
            if let Some(r) = &p.recurrence {
                r.validate().with_context(|| format!("{at} recurrence"))?;
            }
            let duration = p.duration_minutes.unwrap_or(self.defaults.duration_minutes);
            if duration == 0 {
                bail!("{at}: duration_minutes must be more than 0");
            }
            let organizers = p
                .organizers
                .iter()
                .map(|u| {
                    OwnedUserId::try_from(u.as_str())
                        .with_context(|| format!("{at} organizers: {u:?} is no Matrix ID"))
                })
                .collect::<Result<_>>()?;
            out.push(Plenum {
                key: p.key.clone(),
                name: p.name.trim().to_owned(),
                room,
                collection: p.collection.trim().to_owned(),
                recurrence: p.recurrence.clone(),
                prepare_days: p.prepare_days.unwrap_or(self.defaults.prepare_days),
                remind_minutes: p.remind_minutes.unwrap_or(self.defaults.remind_minutes),
                duration_minutes: duration,
                close_after_hours: p
                    .close_after_hours
                    .unwrap_or(self.defaults.close_after_hours),
                organizers,
            });
        }
        Ok(out)
    }

    pub fn public_url(&self) -> String {
        self.outline
            .public_url
            .clone()
            .unwrap_or_else(|| self.outline.url.clone())
            .trim_end_matches('/')
            .to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: &str = r#"
[matrix]
homeserver = "https://matrix.example.org"
user_id = "@plenum-bot:example.org"
device_id = "PLENUM"

[security]
allowed_inviters = []
allowed_rooms = []

[outline]
url = "http://outline:3000"
public_url = "https://wiki.example.org/"
api_key = "ol_api_test"
"#;

    fn parse(extra: &str) -> Config {
        toml::from_str(&format!("{BASE}{extra}")).unwrap()
    }

    #[test]
    fn plenums_get_the_defaults_and_keep_their_own_values() {
        let c = parse(
            r#"
[defaults]
prepare_days = 5

[[plenum]]
key = "haus"
name = "Hausplenum"
room = "!house:example.org"
collection = "Hausplenum"
recurrence = { kind = "monthly_nth", weekday = "sun", nths = [1], time = "19:00" }

[[plenum]]
key = "kueche-eg"
name = "Küchenplenum EG"
room = "!kitchen:example.org"
collection = "Küchenplena"
prepare_days = 1
organizers = ["@anna:example.org"]
"#,
        );
        let ps = c.plenums().unwrap();
        assert_eq!(ps[0].prepare_days, 5);
        assert_eq!(ps[0].remind_minutes, 30);
        assert_eq!(ps[1].prepare_days, 1);
        assert!(ps[1].recurrence.is_none());
        assert_eq!(ps[1].organizers.len(), 1);
        assert_eq!(ps[1].template_title(), "Vorlage Küchenplenum EG");
        assert_eq!(c.public_url(), "https://wiki.example.org");
    }

    #[test]
    fn broken_plenum_tables_are_refused() {
        let one = |body: &str| parse(&format!("[[plenum]]\n{body}")).plenums();
        assert!(one("key = 'Haus'\nname = 'H'\nroom = '!r:x'\ncollection = 'C'").is_err());
        assert!(one("key = 'haus'\nname = 'H'\nroom = 'house'\ncollection = 'C'").is_err());
        assert!(one(
            "key = 'haus'\nname = 'H'\nroom = '!r:x'\ncollection = 'C'\norganizers = ['anna']"
        )
        .is_err());
        assert!(one(
            "key = 'haus'\nname = 'H'\nroom = '!r:x'\ncollection = 'C'\nduration_minutes = 0"
        )
        .is_err());
        let twice = parse(
            "[[plenum]]\nkey = 'a'\nname = 'H'\nroom = '!r:x'\ncollection = 'C'\n\
             [[plenum]]\nkey = 'b'\nname = 'h'\nroom = '!s:x'\ncollection = 'c'",
        );
        assert!(twice.plenums().is_err());
        assert!(parse("").plenums().is_err());
        assert!(toml::from_str::<Config>(&format!(
            "{BASE}[[plenum]]\nkey = 'a'\nname = 'H'\nroom = '!r:x'\ncollection = 'C'\ntypo = 1"
        ))
        .is_err());
    }
}

/// A rendered config from house-infra (`PLENUM_CONFIG_FILE=… cargo test -- --ignored`).
#[cfg(test)]
mod rendered {
    #[test]
    #[ignore = "needs PLENUM_CONFIG_FILE"]
    fn a_rendered_config_is_accepted() {
        let path = std::env::var("PLENUM_CONFIG_FILE").expect("PLENUM_CONFIG_FILE");
        let c: super::Config =
            mxbot_common::config::load_toml(std::path::Path::new(&path)).unwrap();
        let ps = c.plenums().unwrap();
        c.timezone().unwrap();
        for p in &ps {
            println!(
                "{} {:?} {:?}",
                p.key,
                p.recurrence.as_ref().map(|r| r.describe()),
                p.prepare_days
            );
        }
    }
}
