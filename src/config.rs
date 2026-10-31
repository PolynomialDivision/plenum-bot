//! The config file: `[matrix]` and `[security]` (mxbot-common), `[outline]`, `[calendar]`
//! (the plenums' Nextcloud calendars, where the dates are), `[defaults]`, `[pdf]`,
//! `[directory]` (who is in which group), `[members]` (the rooms' members follow the groups)
//! and one `[[plenum]]` table per plenum.

use std::collections::{BTreeSet, HashSet};

use anyhow::{bail, Context, Result};
use chrono_tz::Tz;
use mxbot_common::{
    config::{MatrixConfig, SecurityConfig},
    directory::DirectoryConfig,
    matrix_sdk::ruma::{OwnedRoomId, OwnedUserId},
    membership::Grace,
};
use serde::{Deserialize, Serialize};

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

/// The longest `[members] handover_days` and `handover_max_days`: a year.
pub const HANDOVER_DAYS_MAX: u32 = 365;

/// The rooms of plenums with a group follow it: the chosen account of each member is invited,
/// accounts of nobody in it are removed (mxbot-common `membership`).
#[derive(Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields, default)]
pub struct MembersConfig {
    pub enabled: bool,
    /// Never removed (the house's bots, the room admins of the provisioner); people with
    /// power level 100 in a room are never removed either.
    pub keep: Vec<String>,
    /// Write to a member who has not chosen their Matrix account (nobody new is invited for
    /// them until they do): at most three times, two weeks apart.
    pub ask_to_choose: bool,
    /// Where people choose their account and link others (the identity provider's flow
    /// `house-matrix-accounts`), for that message and the answer to `!link`.
    pub choose_url: Option<String>,
    /// After a member chose an account, their other accounts in a room stay this many days
    /// after the chosen one joined it (mxbot-common `membership::Grace`)…
    pub handover_days: u32,
    /// …and this many days at most after the bot first saw the choice, joined or not.
    pub handover_max_days: u32,
}

impl Default for MembersConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            keep: Vec::new(),
            ask_to_choose: false,
            choose_url: None,
            handover_days: 7,
            handover_max_days: 30,
        }
    }
}

impl MembersConfig {
    /// `choose_url`, trimmed; None when unset or empty.
    pub fn choose_url(&self) -> Option<&str> {
        self.choose_url
            .as_deref()
            .map(str::trim)
            .filter(|u| !u.is_empty())
    }

    /// The handover's lengths.
    pub fn grace(&self) -> Grace {
        Grace {
            after_join: chrono::Duration::days(self.handover_days.into()),
            max: chrono::Duration::days(self.handover_max_days.into()),
        }
    }

    /// Asking people to choose needs the address where they do it; a handover lasts at least
    /// a day and at most [`HANDOVER_DAYS_MAX`] (a typo of many digits would otherwise reach
    /// past any date, and an old account would stay for good), and its longest is no shorter
    /// than the one after joining.
    pub fn check(&self) -> Result<()> {
        if self.ask_to_choose && self.choose_url().is_none() {
            bail!("[members] ask_to_choose needs choose_url (where people choose their account)");
        }
        if self.handover_days == 0 || self.handover_max_days == 0 {
            bail!("[members] handover_days and handover_max_days must be at least 1");
        }
        if self.handover_days > HANDOVER_DAYS_MAX || self.handover_max_days > HANDOVER_DAYS_MAX {
            bail!(
                "[members] handover_days ({}) and handover_max_days ({}) must be at most \
                 {HANDOVER_DAYS_MAX}",
                self.handover_days,
                self.handover_max_days
            );
        }
        if self.handover_days > self.handover_max_days {
            bail!(
                "[members] handover_days ({}) is more than handover_max_days ({})",
                self.handover_days,
                self.handover_max_days
            );
        }
        Ok(())
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

impl CalendarConfig {
    pub fn refresh(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.refresh_minutes.max(1) * 60)
    }
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

/// What of a meeting's minutes goes into the plenum's room. The minutes are made, closed,
/// filed and kept in the wiki the same way in every mode; only the chat differs. Ordered
/// from the most to the least shared.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "lowercase")]
pub enum Publication {
    /// The link before the meeting, a summary after it (decisions, open tasks, who has a
    /// task is mentioned) and the PDF: how it always was.
    #[default]
    Full,
    /// Only the links: the announcement, the reminder and that the minutes are closed, with
    /// the link to them; no decisions, tasks or PDF.
    Link,
    /// Nothing from the wiki: when the meeting is, that it is cancelled or closed, without a
    /// link, an agenda count, a summary or the PDF.
    Confidential,
}

impl Publication {
    /// The word of the chat command (`!minutes confidential`).
    pub fn from_word(word: &str) -> Option<Self> {
        match word.to_lowercase().as_str() {
            "full" | "summary" => Some(Self::Full),
            "link" | "links" => Some(Self::Link),
            "confidential" => Some(Self::Confidential),
            _ => None,
        }
    }

    pub fn word(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::Link => "link",
            Self::Confidential => "confidential",
        }
    }
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
    /// end, unless someone says `!minutes done` earlier.
    pub close_after_hours: u32,
    /// What of a meeting's minutes goes into the chat, unless it is set for the meeting
    /// (`!minutes link`, `!minutes confidential`).
    pub publication: Publication,
}

impl Default for Defaults {
    fn default() -> Self {
        Self {
            timezone: "Europe/Berlin".to_owned(),
            prepare_days: 3,
            remind_minutes: 30,
            duration_minutes: 120,
            close_after_hours: 12,
            publication: Publication::Full,
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
    /// Short name for commands and the state file: `house`, `kitchen-floor-1`. Never renamed
    /// once used: the state, the default calendar and the commands follow it.
    pub key: String,
    /// Shown in titles and messages: "House plenum".
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
    /// Who may close the minutes while the meeting is still on (`!minutes done` before its
    /// end); the bot's admins always may. After the end, everyone in the room may when this
    /// is empty, else only they.
    #[serde(default)]
    pub organizers: Vec<String>,
    /// The directory group the plenum belongs to: its room's members and the wiki's editors
    /// follow it. None: a room nobody manages.
    #[serde(default)]
    pub group: Option<String>,
    /// Only who is also in this directory group counts as a member of `group` (a former
    /// resident still in a kitchen's group is neither kept nor invited). Default for a plenum
    /// with access `members` or `private`: the group of the house plenum (access `house`), if
    /// it has one; `""`: no such rule.
    #[serde(default)]
    pub require_group: Option<String>,
    #[serde(default)]
    pub access: Access,
    /// What of its meetings' minutes goes into its room, unless set for a meeting in the chat
    /// (`full`, `link`, `confidential`); default `[defaults] publication`.
    #[serde(default)]
    pub publication: Option<Publication>,
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
    /// Only the members of `group` who are in this group too count (None: all of them).
    pub require_group: Option<String>,
    pub access: Access,
    /// For a meeting nobody set it for.
    pub publication: Publication,
}

impl Plenum {
    pub fn template_title(&self) -> String {
        format!("{}: Template", self.name)
    }

    pub fn decisions_title(&self) -> String {
        format!("{}: Decisions", self.name)
    }

    pub fn overview_title(&self) -> String {
        format!("{}: Overview", self.name)
    }

    pub fn rules_title(&self) -> String {
        format!("{}: Rules", self.name)
    }

    pub fn protocols_title(&self) -> String {
        format!("{}: Minutes", self.name)
    }

    pub fn example_title(&self) -> String {
        format!("{}: Example minutes", self.name)
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
        // The house plenum's group (who lives here): what the other plenums' groups require.
        let house_groups: BTreeSet<&str> = self
            .plenums
            .iter()
            .filter(|p| p.access == Access::House)
            .filter_map(|p| trimmed(&p.group))
            .collect();
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
            let group = trimmed(&p.group).map(str::to_owned);
            if p.access != Access::House && group.is_none() {
                bail!("{at}: access members or private needs a group (who writes in it)");
            }
            let require_group = match (&p.require_group, &group) {
                (Some(r), None) if !r.trim().is_empty() => {
                    bail!("{at}: require_group needs a group")
                }
                (_, None) => None,
                (Some(r), Some(_)) => Some(r.trim()).filter(|r| !r.is_empty()),
                (None, Some(_)) if p.access == Access::House => None,
                (None, Some(_)) => match house_groups.len() {
                    0 => None,
                    1 => house_groups.first().copied(),
                    _ => bail!(
                        "{at}: the house plenums have several groups ({}): set require_group",
                        house_groups.iter().copied().collect::<Vec<_>>().join(", ")
                    ),
                },
            }
            .filter(|r| group.as_deref() != Some(*r))
            .map(str::to_owned);
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
                require_group,
                access: p.access,
                publication: p.publication.unwrap_or(self.defaults.publication),
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

/// The groups of the house plenums (access `house`): who lives here. One of them existing
/// with nobody in it is a broken directory: no room changes then.
pub fn house_groups(plenums: &[Plenum]) -> BTreeSet<String> {
    plenums
        .iter()
        .filter(|p| p.access == Access::House)
        .filter_map(|p| p.group.clone())
        .collect()
}

/// Every directory group the plenums read: their own and the ones these require.
pub fn directory_groups(plenums: &[Plenum]) -> BTreeSet<String> {
    plenums
        .iter()
        .flat_map(|p| p.group.iter().chain(p.require_group.iter()))
        .cloned()
        .collect()
}

fn trimmed(group: &Option<String>) -> Option<&str> {
    group.as_deref().map(str::trim).filter(|g| !g.is_empty())
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
    fn what_goes_into_the_chat_is_full_unless_the_config_says_otherwise() {
        let plenum = |key: &str, extra: &str| {
            format!(
                "[[plenum]]\nkey = \"{key}\"\nname = \"{key}\"\nroom = \"!{key}:example.org\"\n\
                 collection = \"{key}\"\n{extra}\n"
            )
        };
        let c = parse(&format!(
            "{}{}",
            plenum("house", ""),
            plenum("finance", "publication = \"confidential\"")
        ));
        let ps = c.plenums().unwrap();
        assert_eq!(ps[0].publication, Publication::Full, "as before");
        assert_eq!(ps[1].publication, Publication::Confidential);
        let c = parse(&format!(
            "[defaults]\npublication = \"link\"\n{}",
            plenum("house", "")
        ));
        assert_eq!(c.plenums().unwrap()[0].publication, Publication::Link);
        let wrong = format!("{BASE}{}", plenum("house", "publication = \"secret\""));
        assert!(toml::from_str::<Config>(&wrong).is_err());
        assert!(
            Publication::Full < Publication::Link && Publication::Link < Publication::Confidential
        );
    }

    #[test]
    fn the_example_config_is_accepted() {
        let c: Config = toml::from_str(include_str!("../config.example.toml")).unwrap();
        assert_eq!(c.plenums().unwrap().len(), 2);
        c.timezone().unwrap();
        c.members.check().unwrap();
    }

    #[test]
    fn plenums_get_the_defaults_and_keep_their_own_values() {
        let c = parse(
            r#"
[defaults]
prepare_days = 5

[[plenum]]
key = "house"
name = "House plenum"
room = "!house:example.org"
collection = "House plenum"

[[plenum]]
key = "kitchen-floor-1"
name = "Kitchen plenum 1st floor"
room = "!kitchen:example.org"
collection = "Kitchens"
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
            ("plenum-house", "plenum-kitchen-floor-1")
        );
        assert_eq!(c.calendar.house_group, "residents");
        assert_eq!(
            (c.calendar.password_env.as_str(), c.calendar.refresh_minutes),
            ("CALDAV_PASSWORD", 5)
        );
        assert_eq!(ps[1].organizers.len(), 1);
        assert_eq!(ps[1].template_title(), "Kitchen plenum 1st floor: Template");
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

[members]
keep = ["@calendar-bot:example.org"]

[[plenum]]
key = "kitchen-floor-1"
name = "Kitchen plenum 1st floor"
room = "!k1:example.org"
collection = "Kitchen 1st floor"
group = "kitchen-floor-1"
access = "members"
"#,
        );
        let ps = c.plenums().unwrap();
        assert_eq!(ps[0].group.as_deref(), Some("kitchen-floor-1"));
        assert_eq!(ps[0].access, Access::Members);
        assert_eq!(ps[0].overview_title(), "Kitchen plenum 1st floor: Overview");
        let d = c.directory.as_ref().unwrap();
        assert_eq!(
            (
                d.token_env.as_str(),
                d.refresh_minutes,
                d.server_name.as_deref()
            ),
            ("DIRECTORY_TOKEN", 15, None)
        );
        assert_eq!(c.members.keep.len(), 1);
        // nobody is asked to choose unless the config says so, and where
        assert!(!c.members.ask_to_choose && c.members.choose_url().is_none());
        c.members.check().unwrap();
        let ask = |extra: &str| -> MembersConfig {
            toml::from_str(&format!("ask_to_choose = true\n{extra}")).unwrap()
        };
        assert!(ask("").check().is_err());
        assert!(ask("choose_url = ' '").check().is_err());
        let m = ask("choose_url = ' https://login.example.org/if/flow/house-matrix-accounts/ '");
        m.check().unwrap();
        assert_eq!(
            m.choose_url(),
            Some("https://login.example.org/if/flow/house-matrix-accounts/")
        );
        // the handover: a week after joining, a month at most, unless the config says else
        assert_eq!(m.grace(), Grace::default());
        let grace = |extra: &str| -> Result<Grace> {
            let m: MembersConfig = toml::from_str(extra).unwrap();
            m.check().map(|()| m.grace())
        };
        assert_eq!(
            grace("handover_days = 3\nhandover_max_days = 3").unwrap(),
            Grace {
                after_join: chrono::Duration::days(3),
                max: chrono::Duration::days(3)
            }
        );
        assert!(grace("handover_days = 0").is_err());
        assert!(grace("handover_max_days = 0").is_err());
        assert!(grace("handover_days = 14\nhandover_max_days = 10").is_err());
        // a year at most: more digits than meant would reach past any date
        assert!(grace("handover_days = 365\nhandover_max_days = 365").is_ok());
        assert!(grace("handover_max_days = 366").is_err());
        assert!(grace("handover_days = 4294967295\nhandover_max_days = 4294967295").is_err());
    }

    const DIRECTORY: &str =
        "[directory]\nprovider = 'authentik'\napi_url = 'https://login.example.org/api/v3'\n";

    fn grouped(key: &str, extra: &str) -> String {
        format!(
            "[[plenum]]\nkey = '{key}'\nname = '{key}'\nroom = '!{key}:x'\ncollection = '{key}'\n{extra}\n"
        )
    }

    #[test]
    fn the_team_groups_require_the_house_plenums_group() {
        // as house-infra renders it: the house (access house, residents), kitchens, finance
        let c = parse(&format!(
            "{DIRECTORY}{}{}{}{}",
            grouped("house", "group = 'residents'\naccess = 'house'"),
            grouped(
                "kitchen-floor-1",
                "group = 'kitchen-floor-1'\naccess = 'members'"
            ),
            grouped("finance", "group = 'finance'\naccess = 'private'"),
            grouped("open", ""),
        ));
        let ps = c.plenums().unwrap();
        let req: Vec<Option<&str>> = ps.iter().map(|p| p.require_group.as_deref()).collect();
        assert_eq!(req, [None, Some("residents"), Some("residents"), None]);
        assert_eq!(
            directory_groups(&ps),
            ["residents", "kitchen-floor-1", "finance"]
                .map(String::from)
                .into()
        );
        assert_eq!(house_groups(&ps), ["residents".to_owned()].into());

        // explicit: another group, or "" for no such rule
        let c = parse(&format!(
            "{DIRECTORY}{}{}{}",
            grouped("house", "group = 'residents'"),
            grouped(
                "a",
                "group = 'a'\naccess = 'members'\nrequire_group = 'guests'"
            ),
            grouped("b", "group = 'b'\naccess = 'members'\nrequire_group = ''"),
        ));
        let req: Vec<Option<String>> = c
            .plenums()
            .unwrap()
            .into_iter()
            .map(|p| p.require_group)
            .collect();
        assert_eq!(req, [None, Some("guests".into()), None]);

        // no house plenum with a group: no rule
        let c = parse(&format!(
            "{DIRECTORY}{}",
            grouped("a", "group = 'a'\naccess = 'members'")
        ));
        assert!(c.plenums().unwrap()[0].require_group.is_none());
        // its own group is no rule either
        let c = parse(&format!(
            "{DIRECTORY}{}{}",
            grouped("house", "group = 'residents'"),
            grouped("all", "group = 'residents'\naccess = 'members'"),
        ));
        assert!(c.plenums().unwrap()[1].require_group.is_none());

        // two house plenums with different groups: which one is meant must be said
        let two = |extra: &str| {
            parse(&format!(
                "{DIRECTORY}{}{}{}",
                grouped("house", "group = 'residents'"),
                grouped("work", "group = 'workers'"),
                grouped("a", &format!("group = 'a'\naccess = 'members'{extra}")),
            ))
            .plenums()
        };
        assert!(two("").is_err());
        assert_eq!(
            two("\nrequire_group = 'residents'").unwrap()[2]
                .require_group
                .as_deref(),
            Some("residents")
        );
        // a rule without a group of its own
        assert!(parse(&format!(
            "{DIRECTORY}{}",
            grouped("a", "require_group = 'residents'")
        ))
        .plenums()
        .is_err());
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
        assert!(one("key = 'House'\nname = 'H'\nroom = '!r:x'\ncollection = 'C'").is_err());
        assert!(one("key = 'house'\nname = 'H'\nroom = 'house'\ncollection = 'C'").is_err());
        assert!(one(
            "key = 'house'\nname = 'H'\nroom = '!r:x'\ncollection = 'C'\norganizers = ['anna']"
        )
        .is_err());
        assert!(one(
            "key = 'house'\nname = 'H'\nroom = '!r:x'\ncollection = 'C'\nduration_minutes = 0"
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
            "key = 'a'\nname = 'A'\nroom = '!r:x'\ncollection = 'C'\ncalendar = 'House Plenum'"
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
            println!(
                "{} {} {:?} {:?} {:?}",
                p.key, p.calendar, p.prepare_days, p.group, p.require_group
            );
        }
    }
}
