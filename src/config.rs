//! The config file: `[matrix]` and `[security]` (mxbot-common), `[outline]`, `[calendar]`
//! (the plenums' Nextcloud calendars, where the dates are), `[defaults]`, `[pdf]`,
//! `[directory]` (who is in which group), `[members]` (the rooms' members follow the groups)
//! and one `[[plenum]]` table per plenum.

use std::collections::HashSet;

use anyhow::{bail, Context, Result};
use chrono_tz::Tz;
use mxbot_common::{
    config::{MatrixConfig, SecurityConfig},
    matrix_sdk::ruma::{OwnedRoomId, OwnedUserId},
};
use serde::Deserialize;

/// Environment variable with the Outline API key (overrides `[outline] api_key`).
pub const API_KEY_ENV: &str = "OUTLINE_API_KEY";

#[derive(Deserialize)]
pub struct Config {
    pub matrix: MatrixConfig,
    #[serde(default)]
    pub security: SecurityConfig,
    pub outline: OutlineConfig,
    pub calendar: CalendarConfig,
    #[serde(default)]
    pub defaults: Defaults,
    #[serde(default)]
    pub pdf: PdfConfig,
    #[serde(default)]
    pub directory: Option<DirectoryConfig>,
    #[serde(default)]
    pub members: MembersConfig,
    #[serde(rename = "plenum", default)]
    pub plenums: Vec<PlenumConfig>,
}

/// The house's directory (its identity provider): who is in a plenum's group.
#[derive(Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct DirectoryConfig {
    pub provider: DirectoryProvider,
    /// The REST API base, e.g. "https://login.example.org/api/v3".
    pub api_url: String,
    /// The house's homeserver: a resident's house account is @<login>:<server_name>.
    pub server_name: String,
    /// User attribute that overrides the login as the house account's localpart.
    #[serde(default)]
    pub localpart_attribute: Option<String>,
    /// User attribute with a person's other accounts: a list of full Matrix IDs.
    #[serde(default = "default_matrix_ids_attribute")]
    pub matrix_ids_attribute: String,
    /// Environment variable with the read-only API token (never in the config).
    #[serde(default = "default_directory_token_env")]
    pub token_env: String,
    #[serde(default = "default_refresh_minutes")]
    pub refresh_minutes: u64,
}

#[derive(Deserialize, Clone, Copy, Debug, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum DirectoryProvider {
    Authentik,
}

fn default_matrix_ids_attribute() -> String {
    "matrix_ids".to_owned()
}

fn default_directory_token_env() -> String {
    "DIRECTORY_TOKEN".to_owned()
}

fn default_refresh_minutes() -> u64 {
    15
}

/// The rooms of plenums with a group follow it: its members are invited, others removed.
#[derive(Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields, default)]
pub struct MembersConfig {
    pub enabled: bool,
    /// Never removed (the house's bots, the room admins of the provisioner); people with
    /// power level 100 in a room are never removed either.
    pub keep: Vec<String>,
}

impl Default for MembersConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            keep: Vec::new(),
        }
    }
}

/// The plenums' calendars in Nextcloud (CalDAV), owned by the bot's own account.
#[derive(Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct CalendarConfig {
    /// The bot's calendar home: `https://cloud.example.org/remote.php/dav/calendars/plenum-bot/`.
    pub url: String,
    pub username: String,
    /// Environment variable with the account's app password (never in the config).
    #[serde(default = "default_caldav_password_env")]
    pub password_env: String,
    /// How often the calendars are read again.
    #[serde(default = "default_calendar_refresh")]
    pub refresh_minutes: u64,
    /// Make missing calendars and keep their shares (false: only read them).
    #[serde(default = "yes")]
    pub manage: bool,
    /// The Nextcloud group of everyone in the house (edits the house plenum's calendar, reads
    /// the kitchens').
    #[serde(default = "default_house_group")]
    pub house_group: String,
    /// Nextcloud users who read every calendar that is not private (calendar-bot).
    #[serde(default)]
    pub readers: Vec<String>,
    /// Where people open the calendars (the Nextcloud calendar app), for the links.
    #[serde(default)]
    pub web_url: String,
}

fn default_caldav_password_env() -> String {
    "CALDAV_PASSWORD".to_owned()
}

fn default_calendar_refresh() -> u64 {
    5
}

fn yes() -> bool {
    true
}

fn default_house_group() -> String {
    "residents".to_owned()
}

/// Who may read and write a plenum's collection in the wiki.
#[derive(Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Access {
    /// Everyone in the wiki reads and writes (the house plenum).
    #[default]
    House,
    /// Everyone reads, the plenum's group writes (a kitchen).
    Members,
    /// Only the plenum's group sees it (the finance group); not even the wiki's admins, unless
    /// they are in the group (checked against Outline 1.11.0, 2026-10-22).
    Private,
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
    /// Its calendar in the bot's Nextcloud account (the URL's last part); default
    /// `plenum-<key>`. Never renamed once used: a new name is a new, empty calendar.
    #[serde(default)]
    pub calendar: Option<String>,
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
    /// The directory group the plenum belongs to: its room's members and the wiki's editors
    /// follow it. None: a room nobody manages (the house chat).
    #[serde(default)]
    pub group: Option<String>,
    #[serde(default)]
    pub access: Access,
}

/// A plenum with every default applied and every ID parsed.
#[derive(Clone, Debug)]
pub struct Plenum {
    pub key: String,
    pub name: String,
    pub room: OwnedRoomId,
    pub collection: String,
    /// The calendar's slug in the bot's account.
    pub calendar: String,
    pub prepare_days: u32,
    pub remind_minutes: u32,
    pub duration_minutes: u32,
    pub close_after_hours: u32,
    pub organizers: HashSet<OwnedUserId>,
    pub group: Option<String>,
    pub access: Access,
}

impl Plenum {
    pub fn template_title(&self) -> String {
        format!("Vorlage {}", self.name)
    }

    pub fn decisions_title(&self) -> String {
        format!("Beschlüsse {}", self.name)
    }

    pub fn overview_title(&self) -> String {
        format!("Übersicht {}", self.name)
    }

    pub fn rules_title(&self) -> String {
        format!("Regeln {}", self.name)
    }

    pub fn protocols_title(&self) -> String {
        format!("Protokolle {}", self.name)
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

    /// The calendar account's app password, from the environment.
    pub fn caldav_password(&self) -> Result<String> {
        let env = &self.calendar.password_env;
        match std::env::var(env) {
            Ok(p) if !p.trim().is_empty() => Ok(p.trim().to_owned()),
            _ => bail!("no calendar password: set {env}"),
        }
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
        let mut calendars = HashSet::new();
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
            if p.group.is_some() && self.directory.is_none() {
                bail!("{at}: a group needs [directory] (where the bot reads who is in it)");
            }
            if !titles.insert((
                p.collection.trim().to_lowercase(),
                p.name.trim().to_lowercase(),
            )) {
                bail!("{at}: another plenum has the same name in the same collection");
            }
            let room = mxbot_common::rooms::parse_room_id(&format!("{at} room"), &p.room)?;
            let calendar = p
                .calendar
                .clone()
                .unwrap_or_else(|| format!("plenum-{}", p.key));
            if calendar.is_empty()
                || !calendar
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
            {
                bail!("{at}: calendar must be lowercase letters, digits and '-'");
            }
            if !calendars.insert(calendar.clone()) {
                bail!("{at}: another plenum has the calendar {calendar}");
            }
            let duration = p.duration_minutes.unwrap_or(self.defaults.duration_minutes);
            if duration == 0 {
                bail!("{at}: duration_minutes must be more than 0");
            }
            let group = p
                .group
                .as_deref()
                .map(str::trim)
                .filter(|g| !g.is_empty())
                .map(str::to_owned);
            if p.access != Access::House && group.is_none() {
                bail!("{at}: access members or private needs a group (who writes in it)");
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
                calendar,
                prepare_days: p.prepare_days.unwrap_or(self.defaults.prepare_days),
                remind_minutes: p.remind_minutes.unwrap_or(self.defaults.remind_minutes),
                duration_minutes: duration,
                close_after_hours: p
                    .close_after_hours
                    .unwrap_or(self.defaults.close_after_hours),
                organizers,
                group,
                access: p.access,
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

[calendar]
url = "https://cloud.example.org/remote.php/dav/calendars/plenum-bot/"
username = "plenum-bot"
readers = ["calendar-bot"]
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
        assert_eq!(
            (ps[0].calendar.as_str(), ps[1].calendar.as_str()),
            ("plenum-haus", "plenum-kueche-eg")
        );
        assert_eq!(c.calendar.house_group, "residents");
        assert_eq!(
            (c.calendar.password_env.as_str(), c.calendar.refresh_minutes),
            ("CALDAV_PASSWORD", 5)
        );
        assert_eq!(ps[1].organizers.len(), 1);
        assert_eq!(ps[1].template_title(), "Vorlage Küchenplenum EG");
        assert_eq!(c.public_url(), "https://wiki.example.org");
        assert_eq!(ps[0].access, Access::House);
        assert!(ps[0].group.is_none());
        assert!(c.members.enabled && c.directory.is_none());
    }

    #[test]
    fn a_group_plenum_with_the_directory_and_its_feeds() {
        let c = parse(
            r#"
[directory]
provider = "authentik"
api_url = "https://login.example.org/api/v3"
server_name = "example.org"

[members]
keep = ["@calendar-bot:example.org"]

[[plenum]]
key = "kitchen-floor-1"
name = "Küchenplenum 1. OG"
room = "!k1:example.org"
collection = "Küche 1. OG"
group = "kitchen-floor-1"
access = "members"
"#,
        );
        let ps = c.plenums().unwrap();
        assert_eq!(ps[0].group.as_deref(), Some("kitchen-floor-1"));
        assert_eq!(ps[0].access, Access::Members);
        assert_eq!(ps[0].overview_title(), "Übersicht Küchenplenum 1. OG");
        let d = c.directory.as_ref().unwrap();
        assert_eq!(
            (d.token_env.as_str(), d.refresh_minutes),
            ("DIRECTORY_TOKEN", 15)
        );
        assert_eq!(c.members.keep.len(), 1);
    }

    #[test]
    fn a_closed_collection_needs_a_group_and_a_group_the_directory() {
        let one = |body: &str| {
            parse(&format!(
                "[[plenum]]\nkey = 'f'\nname = 'F'\nroom = '!r:x'\ncollection = 'C'\n{body}"
            ))
            .plenums()
        };
        assert!(one("access = 'private'").is_err());
        assert!(one("access = 'members'").is_err());
        assert!(one("group = 'finance'").is_err()); // no [directory]
        assert!(toml::from_str::<Config>(&format!("{BASE}[[plenum]]\nkey = 'f'\nname = 'F'\nroom = '!r:x'\ncollection = 'C'\naccess = 'secret'")).is_err());
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
        // two plenums on one calendar, a calendar name that is no URL part, a leftover rule
        let shared = parse(
            "[[plenum]]\nkey = 'a'\nname = 'A'\nroom = '!r:x'\ncollection = 'C'\ncalendar = 'cal'\n\
             [[plenum]]\nkey = 'b'\nname = 'B'\nroom = '!s:x'\ncollection = 'C'\ncalendar = 'cal'",
        );
        assert!(shared.plenums().is_err());
        assert!(one(
            "key = 'a'\nname = 'A'\nroom = '!r:x'\ncollection = 'C'\ncalendar = 'Haus Plenum'"
        )
        .is_err());
        assert!(toml::from_str::<Config>(&format!(
            "{BASE}[[plenum]]\nkey = 'a'\nname = 'H'\nroom = '!r:x'\ncollection = 'C'\nrecurrence = {{ kind = 'weekly' }}"
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
            println!("{} {} {:?}", p.key, p.calendar, p.prepare_days);
        }
    }
}
