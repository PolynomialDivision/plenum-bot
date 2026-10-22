//! The plenums' dates: one Nextcloud calendar per plenum, the one place they are kept
//! (decided 2026-10-22). People create, move, cancel and repeat meetings there; the bot reads
//! the calendar over CalDAV and follows it. Every occurrence keeps the ID the calendar gives it
//! (the event's UID, for a series with the date it was first due), so a meeting that moves
//! keeps its protocol.
//!
//! The bot's own Nextcloud account owns the calendars: it makes a missing one and keeps who
//! may see and edit it in line with the plenum's access (`shares`): the house plenum is
//! edited by the house group; a kitchen's by its group and read by the house; a private one
//! (the Finanz-AG) only by its group. The readers of `[calendar] readers` (calendar-bot) read
//! every calendar that is not private. Nothing else is shared: a share made by hand goes.

use std::collections::BTreeMap;

use anyhow::{bail, Context, Result};
use chrono::{NaiveDate, NaiveDateTime};
use mxbot_common::calendar::{
    caldav,
    ics::{self, EntryTime, IcsEvent},
};
use serde::{Deserialize, Serialize};

use crate::{config::Access, schedule::MeetingKey};

/// One meeting as the calendar has it now.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Occurrence {
    /// Stable when it is moved: the UID of a single event, the UID and original date of one
    /// of a series.
    pub id: String,
    pub start: MeetingKey,
    /// Its local end, if the event has one.
    #[serde(default)]
    pub end: Option<NaiveDateTime>,
    #[serde(default)]
    pub summary: String,
}

/// The timed occurrences from `from` to `to` (local days), each once (all-day events are no
/// meetings and are left out).
pub fn occurrences(events: &[IcsEvent], from: NaiveDate, to: NaiveDate) -> Vec<Occurrence> {
    let days = (to - from).num_days() + 1;
    let mut found: BTreeMap<String, Occurrence> = BTreeMap::new();
    for (_, entries) in ics::expand(events, from, days.max(1)) {
        for e in entries {
            let EntryTime::Timed { end, .. } = e.time else {
                continue;
            };
            found.entry(e.key.clone()).or_insert_with(|| Occurrence {
                id: e.key.clone(),
                start: MeetingKey(e.starts_at),
                end: end.map(|t| e.last_day.and_time(t)),
                summary: e.summary.clone(),
            });
        }
    }
    let mut list: Vec<Occurrence> = found.into_values().collect();
    list.sort_by(|a, b| a.start.cmp(&b.start).then_with(|| a.id.cmp(&b.id)));
    list
}

/// Someone a calendar is shared with: `principal:principals/groups/<gid>` or
/// `principal:principals/users/<uid>`, and whether they may edit it.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Share {
    pub principal: String,
    pub write: bool,
}

pub fn group(gid: &str) -> String {
    format!("principal:principals/groups/{gid}")
}

pub fn user(uid: &str) -> String {
    format!("principal:principals/users/{uid}")
}

/// Who a plenum's calendar is shared with.
pub fn wanted_shares(
    access: Access,
    plenum_group: Option<&str>,
    house_group: &str,
    readers: &[String],
) -> Vec<Share> {
    let mut out = Vec::new();
    let share = |principal: String, write: bool| Share { principal, write };
    match access {
        Access::House => out.push(share(group(house_group), true)),
        Access::Members => {
            if let Some(g) = plenum_group {
                out.push(share(group(g), true));
            }
            out.push(share(group(house_group), false));
        }
        Access::Private => {
            if let Some(g) = plenum_group {
                out.push(share(group(g), true));
            }
        }
    }
    if access != Access::Private {
        out.extend(readers.iter().map(|r| share(user(r), false)));
    }
    // a group that is also the house group (a plenum of everyone) is shared once, writing
    out.sort_by(|a, b| a.principal.cmp(&b.principal).then(b.write.cmp(&a.write)));
    out.dedup_by(|later, kept| later.principal == kept.principal);
    out
}

/// `(set, remove)`: shares to make or change, principals to take away.
pub fn plan_shares(current: &[Share], wanted: &[Share]) -> (Vec<Share>, Vec<String>) {
    let set = wanted
        .iter()
        .filter(|w| !current.contains(w))
        .cloned()
        .collect();
    let remove = current
        .iter()
        .filter(|c| !wanted.iter().any(|w| w.principal == c.principal))
        .map(|c| c.principal.clone())
        .collect();
    (set, remove)
}

/// The text between `<prefix:name ...>` and its closing tag, for every element with that
/// local name (whatever namespace prefix the server chose).
fn elements<'a>(xml: &'a str, name: &str) -> Vec<&'a str> {
    let mut out = Vec::new();
    let mut rest = xml;
    while let Some(open) = find_tag(rest, name) {
        let after = &rest[open..];
        let Some(gt) = after.find('>') else {
            break;
        };
        if after[..gt].ends_with('/') {
            out.push("");
            rest = &after[gt + 1..];
            continue;
        }
        let body = &after[gt + 1..];
        let Some(close) = find_close(body, name) else {
            break;
        };
        out.push(&body[..close]);
        rest = &body[close..];
    }
    out
}

/// The position of the next `<name` or `<prefix:name` start tag.
fn find_tag(xml: &str, name: &str) -> Option<usize> {
    let mut from = 0;
    while let Some(i) = xml[from..].find('<') {
        let at = from + i;
        let tag = &xml[at + 1..];
        let local = tag
            .split(|c: char| c.is_whitespace() || c == '>' || c == '/')
            .next()
            .unwrap_or("");
        let local = local.rsplit(':').next().unwrap_or(local);
        if !tag.starts_with('/') && local == name {
            return Some(at);
        }
        from = at + 1;
    }
    None
}

fn find_close(xml: &str, name: &str) -> Option<usize> {
    let mut from = 0;
    while let Some(i) = xml[from..].find("</") {
        let at = from + i;
        let tag = &xml[at + 2..];
        let local = tag.split('>').next().unwrap_or("");
        if local.rsplit(':').next() == Some(name) {
            return Some(at);
        }
        from = at + 2;
    }
    None
}

/// The shares in a PROPFIND answer with `oc:invite` (Nextcloud: one `user` element per
/// sharee, `read-write` among its access when it may edit).
pub fn parse_shares(xml: &str) -> Vec<Share> {
    let mut out: Vec<Share> = elements(xml, "invite")
        .into_iter()
        .flat_map(|invite| elements(invite, "user"))
        .filter_map(|u| {
            let href = elements(u, "href").first()?.trim().to_owned();
            let write = !elements(u, "read-write").is_empty();
            Some(Share {
                principal: href,
                write,
            })
        })
        .collect();
    out.sort();
    out
}

/// The bot's Nextcloud account over CalDAV: `account` is its calendar home
/// (`https://cloud.example.org/remote.php/dav/calendars/plenum-bot/`).
pub struct Nextcloud {
    http: reqwest::Client,
    account: String,
    username: String,
    password: String,
}

fn method(name: &str) -> reqwest::Method {
    reqwest::Method::from_bytes(name.as_bytes()).expect("a valid method")
}

impl Nextcloud {
    pub fn new(account: &str, username: &str, password: &str) -> Result<Self> {
        Ok(Self {
            http: reqwest::Client::builder()
                .user_agent(concat!("plenum-bot/", env!("CARGO_PKG_VERSION")))
                .timeout(std::time::Duration::from_secs(30))
                .build()?,
            account: format!("{}/", account.trim_end_matches('/')),
            username: username.to_owned(),
            password: password.to_owned(),
        })
    }

    pub fn calendar_url(&self, slug: &str) -> String {
        format!("{}{slug}/", self.account)
    }

    async fn dav(
        &self,
        m: &str,
        url: &str,
        depth: Option<&str>,
        body: String,
    ) -> Result<(u16, String)> {
        let mut req = self
            .http
            .request(method(m), url)
            .basic_auth(&self.username, Some(&self.password))
            .header("Content-Type", "application/xml; charset=utf-8")
            .body(body);
        if let Some(d) = depth {
            req = req.header("Depth", d);
        }
        let resp = req.send().await.with_context(|| format!("{m} {url}"))?;
        let status = resp.status().as_u16();
        Ok((status, resp.text().await.unwrap_or_default()))
    }

    /// The calendar `slug` with this display name; made when missing. True when it was made.
    pub async fn ensure_calendar(&self, slug: &str, name: &str) -> Result<bool> {
        let url = self.calendar_url(slug);
        let (status, body) = self
            .dav(
                "PROPFIND",
                &url,
                Some("0"),
                r#"<?xml version="1.0" encoding="utf-8"?><d:propfind xmlns:d="DAV:"><d:prop><d:displayname/></d:prop></d:propfind>"#.to_owned(),
            )
            .await?;
        match status {
            207 => {
                let current = elements(&body, "displayname")
                    .first()
                    .map(|s| s.trim().to_owned());
                if current.as_deref() != Some(name) {
                    let (status, _) = self
                        .dav(
                            "PROPPATCH",
                            &url,
                            None,
                            format!(r#"<?xml version="1.0" encoding="utf-8"?><d:propertyupdate xmlns:d="DAV:"><d:set><d:prop><d:displayname>{}</d:displayname></d:prop></d:set></d:propertyupdate>"#, xml_text(name)),
                        )
                        .await?;
                    if status != 207 {
                        bail!("renaming the calendar {slug}: {status}");
                    }
                }
                Ok(false)
            }
            404 => {
                let (status, body) = self
                    .dav(
                        "MKCALENDAR",
                        &url,
                        None,
                        format!(r#"<?xml version="1.0" encoding="utf-8"?><c:mkcalendar xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav"><d:set><d:prop><d:displayname>{}</d:displayname><c:supported-calendar-component-set><c:comp name="VEVENT"/></c:supported-calendar-component-set></d:prop></d:set></c:mkcalendar>"#, xml_text(name)),
                    )
                    .await?;
                if status != 201 {
                    bail!(
                        "making the calendar {slug}: {status} {}",
                        body.chars().take(200).collect::<String>()
                    );
                }
                Ok(true)
            }
            other => bail!(
                "the calendar {slug}: {other} {}",
                body.chars().take(200).collect::<String>()
            ),
        }
    }

    pub async fn shares(&self, slug: &str) -> Result<Vec<Share>> {
        let (status, body) = self
            .dav(
                "PROPFIND",
                &self.calendar_url(slug),
                Some("0"),
                r#"<?xml version="1.0" encoding="utf-8"?><d:propfind xmlns:d="DAV:" xmlns:oc="http://owncloud.org/ns"><d:prop><oc:invite/></d:prop></d:propfind>"#.to_owned(),
            )
            .await?;
        if status != 207 {
            bail!("the shares of {slug}: {status}");
        }
        Ok(parse_shares(&body))
    }

    async fn share_request(&self, slug: &str, inner: String) -> Result<()> {
        let (status, body) = self
            .dav(
                "POST",
                &self.calendar_url(slug),
                None,
                format!(r#"<?xml version="1.0" encoding="utf-8"?><oc:share xmlns:d="DAV:" xmlns:oc="http://owncloud.org/ns">{inner}</oc:share>"#),
            )
            .await?;
        if !(200..300).contains(&status) {
            bail!(
                "sharing {slug}: {status} {}",
                body.chars().take(200).collect::<String>()
            );
        }
        Ok(())
    }

    pub async fn share(&self, slug: &str, share: &Share) -> Result<()> {
        let write = if share.write { "<oc:read-write/>" } else { "" };
        self.share_request(
            slug,
            format!(
                "<oc:set><d:href>{}</d:href>{write}</oc:set>",
                xml_text(&share.principal)
            ),
        )
        .await
    }

    pub async fn unshare(&self, slug: &str, principal: &str) -> Result<()> {
        self.share_request(
            slug,
            format!(
                "<oc:remove><d:href>{}</d:href></oc:remove>",
                xml_text(principal)
            ),
        )
        .await
    }

    /// The calendar's events touching `from`..`to` (a failure is an error, never "none").
    pub async fn events(
        &self,
        slug: &str,
        from: NaiveDate,
        to: NaiveDate,
    ) -> Result<Vec<IcsEvent>> {
        caldav::fetch_calendar(
            &self.http,
            &self.calendar_url(slug),
            &self.username,
            &self.password,
            from,
            to,
        )
        .await
        .with_context(|| format!("reading the calendar {slug}"))
    }
}

fn xml_text(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(y: i32, m: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, day).unwrap()
    }

    const SERIES: &str = "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:plenum-1\r\nSUMMARY:Hausplenum\r\n\
DTSTART;TZID=Europe/Berlin:20261101T190000\r\nDTEND;TZID=Europe/Berlin:20261101T210000\r\n\
RRULE:FREQ=MONTHLY;BYDAY=1SU\r\nEXDATE;TZID=Europe/Berlin:20270103T190000\r\nEND:VEVENT\r\n\
BEGIN:VEVENT\r\nUID:plenum-1\r\nRECURRENCE-ID;TZID=Europe/Berlin:20261206T190000\r\nSUMMARY:Hausplenum\r\n\
DTSTART;TZID=Europe/Berlin:20261208T200000\r\nDTEND;TZID=Europe/Berlin:20261208T220000\r\nEND:VEVENT\r\n\
BEGIN:VEVENT\r\nUID:allday\r\nSUMMARY:Putztag\r\nDTSTART;VALUE=DATE:20261110\r\nEND:VEVENT\r\n\
BEGIN:VEVENT\r\nUID:extra\r\nSUMMARY:Sonderplenum\r\nDTSTART;TZID=Europe/Berlin:20261115T180000\r\n\
END:VEVENT\r\nEND:VCALENDAR\r\n";

    #[test]
    fn a_series_with_a_moved_and_an_excluded_occurrence() {
        let occ = occurrences(&ics::parse_ics(SERIES), d(2026, 10, 1), d(2027, 2, 28));
        let list: Vec<(String, String)> = occ
            .iter()
            .map(|o| (o.id.clone(), o.start.to_string()))
            .collect();
        assert_eq!(
            list,
            vec![
                (
                    "plenum-1#2026-11-01".to_owned(),
                    "2026-11-01 19:00".to_owned()
                ),
                ("extra".to_owned(), "2026-11-15 18:00".to_owned()),
                // moved from the 6th: same ID, new start
                (
                    "plenum-1#2026-12-06".to_owned(),
                    "2026-12-08 20:00".to_owned()
                ),
                // 3 January excluded
                (
                    "plenum-1#2027-02-07".to_owned(),
                    "2027-02-07 19:00".to_owned()
                ),
            ]
        );
        assert_eq!(occ[0].end.unwrap().to_string(), "2026-11-01 21:00:00");
        assert_eq!(occ[1].end, None);
    }

    #[test]
    fn the_house_plenums_series_has_its_fifth_sundays() {
        let ics = std::fs::read_to_string(concat!(
            env!("HOME"),
            "/unraid/house-infra/docs/plenum/hausplenum.ics"
        ));
        let Ok(ics) = ics else { return }; // house-infra not next to the bot
        let occ = occurrences(&ics::parse_ics(&ics), d(2026, 11, 1), d(2026, 11, 30));
        let starts: Vec<String> = occ.iter().map(|o| o.start.to_string()).collect();
        assert_eq!(
            starts,
            ["2026-11-01 19:00", "2026-11-15 19:00", "2026-11-29 19:00"]
        );
    }

    #[test]
    fn shares_follow_the_access() {
        let readers = vec!["calendar-bot".to_owned()];
        let w = |p: &str, write| Share {
            principal: p.to_owned(),
            write,
        };
        assert_eq!(
            wanted_shares(Access::House, None, "residents", &readers),
            vec![
                w("principal:principals/groups/residents", true),
                w("principal:principals/users/calendar-bot", false)
            ]
        );
        assert_eq!(
            wanted_shares(
                Access::Members,
                Some("kitchen-floor-1"),
                "residents",
                &readers
            ),
            vec![
                w("principal:principals/groups/kitchen-floor-1", true),
                w("principal:principals/groups/residents", false),
                w("principal:principals/users/calendar-bot", false)
            ]
        );
        assert_eq!(
            wanted_shares(Access::Private, Some("finance"), "residents", &readers),
            vec![w("principal:principals/groups/finance", true)]
        );
        // a members plenum of the house group itself: shared once, writing
        assert_eq!(
            wanted_shares(Access::Members, Some("residents"), "residents", &[]),
            vec![w("principal:principals/groups/residents", true)]
        );
    }

    #[test]
    fn a_share_plan_changes_only_what_differs() {
        let w = |p: &str, write| Share {
            principal: p.to_owned(),
            write,
        };
        let current = [
            w("principal:principals/groups/finance", false),
            w("principal:principals/users/eve", true),
        ];
        let wanted = [w("principal:principals/groups/finance", true)];
        let (set, remove) = plan_shares(&current, &wanted);
        assert_eq!(set, vec![w("principal:principals/groups/finance", true)]);
        assert_eq!(remove, vec!["principal:principals/users/eve".to_owned()]);
        assert_eq!(plan_shares(&wanted, &wanted), (vec![], vec![]));
    }

    #[test]
    fn shares_are_read_whatever_the_prefix() {
        let xml = r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:" xmlns:x1="http://owncloud.org/ns"><d:response><d:href>/remote.php/dav/calendars/plenum-bot/plenum-haus/</d:href>
<d:propstat><d:prop><x1:invite>
<x1:user><d:href>principal:principals/groups/residents</d:href><x1:common-name>Bewohner</x1:common-name><x1:invite-accepted/><x1:access><x1:read-write/></x1:access></x1:user>
<x1:user><d:href>principal:principals/users/calendar-bot</d:href><x1:invite-accepted/><x1:access><x1:read/></x1:access></x1:user>
</x1:invite></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response></d:multistatus>"#;
        assert_eq!(
            parse_shares(xml),
            vec![
                Share {
                    principal: "principal:principals/groups/residents".into(),
                    write: true
                },
                Share {
                    principal: "principal:principals/users/calendar-bot".into(),
                    write: false
                },
            ]
        );
        assert!(parse_shares(
            "<d:multistatus xmlns:d=\"DAV:\"><oc:invite xmlns:oc=\"x\"/></d:multistatus>"
        )
        .is_empty());
    }
}

/// Against a real Nextcloud (`cargo test -- --ignored a_calendar_in_a_real_nextcloud`):
/// `NEXTCLOUD_IT_DAV` (`http://…/remote.php/dav`) and app passwords in
/// `NEXTCLOUD_IT_PASS_<USER>` for plenum-bot, calendar-bot, anna (residents, kitchen-floor-1),
/// ben (residents) and eve (no group). house-infra's `KEEP=1 make test-cloud` makes them.
#[cfg(test)]
mod against_nextcloud {
    use super::*;
    use crate::config::Access;

    fn pass(user: &str) -> String {
        std::env::var(format!(
            "NEXTCLOUD_IT_PASS_{}",
            user.to_uppercase().replace('-', "_")
        ))
        .unwrap_or_else(|_| panic!("NEXTCLOUD_IT_PASS for {user}"))
    }

    async fn request(
        m: &str,
        url: &str,
        user: &str,
        body: &str,
        ctype: &str,
        depth: Option<&str>,
    ) -> (u16, String) {
        let mut r = reqwest::Client::new()
            .request(method(m), url)
            .basic_auth(user, Some(pass(user)))
            .header("Content-Type", ctype)
            .body(body.to_owned());
        if let Some(d) = depth {
            r = r.header("Depth", d);
        }
        let resp = r.send().await.unwrap();
        (resp.status().as_u16(), resp.text().await.unwrap())
    }

    /// Where `user` finds a calendar shared with them (its URL), if they do.
    async fn shared_url(dav: &str, user: &str, slug: &str) -> Option<String> {
        let home = format!("{dav}/calendars/{user}/");
        let (status, xml) = request(
            "PROPFIND",
            &home,
            user,
            r#"<d:propfind xmlns:d="DAV:"><d:prop><d:resourcetype/></d:prop></d:propfind>"#,
            "application/xml",
            Some("1"),
        )
        .await;
        assert_eq!(status, 207, "{xml}");
        let href = elements(&xml, "href").into_iter().find(|h| {
            h.trim_end_matches('/')
                .ends_with(&format!("{slug}_shared_by_plenum-bot"))
        })?;
        let base = reqwest::Url::parse(dav).unwrap();
        Some(base.join(href.trim()).unwrap().to_string())
    }

    const SERIES: &str = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//it//EN\r\nBEGIN:VEVENT\r\nUID:it-kitchen\r\n\
DTSTAMP:20261020T100000Z\r\nSUMMARY:Küchenplenum\r\nDTSTART;TZID=Europe/Berlin:20261102T200000\r\n\
DTEND;TZID=Europe/Berlin:20261102T210000\r\nRRULE:FREQ=WEEKLY;BYDAY=MO;COUNT=5\r\n\
EXDATE;TZID=Europe/Berlin:20261116T200000\r\nEND:VEVENT\r\nBEGIN:VEVENT\r\nUID:it-kitchen\r\n\
DTSTAMP:20261020T100000Z\r\nRECURRENCE-ID;TZID=Europe/Berlin:20261109T200000\r\nSUMMARY:Küchenplenum\r\n\
DTSTART;TZID=Europe/Berlin:20261110T210000\r\nDTEND;TZID=Europe/Berlin:20261110T220000\r\nEND:VEVENT\r\n\
END:VCALENDAR\r\n";

    #[tokio::test]
    #[ignore = "needs a running Nextcloud"]
    async fn a_calendar_in_a_real_nextcloud() {
        let dav = std::env::var("NEXTCLOUD_IT_DAV").expect("NEXTCLOUD_IT_DAV");
        let dav = dav.trim_end_matches('/').to_owned();
        let run = uuid::Uuid::new_v4().to_string()[..8].to_owned();
        let (kitchen, finance) = (
            format!("plenum-it-kitchen-{run}"),
            format!("plenum-it-finance-{run}"),
        );
        let nc = Nextcloud::new(
            &format!("{dav}/calendars/plenum-bot/"),
            "plenum-bot",
            &pass("plenum-bot"),
        )
        .unwrap();
        let readers = vec!["calendar-bot".to_owned()];

        // made once, renamed when the name changes
        assert!(nc.ensure_calendar(&kitchen, "IT Küche").await.unwrap());
        assert!(!nc.ensure_calendar(&kitchen, "IT Küche").await.unwrap());
        assert!(!nc
            .ensure_calendar(&kitchen, "IT Küche 1. OG")
            .await
            .unwrap());
        assert!(nc.ensure_calendar(&finance, "IT Finanz-AG").await.unwrap());

        // shares as the access says; a share by hand goes
        for (slug, access, group) in [
            (&kitchen, Access::Members, "kitchen-floor-1"),
            (&finance, Access::Private, "finance"),
        ] {
            let wanted = wanted_shares(access, Some(group), "residents", &readers);
            nc.share(
                slug,
                &Share {
                    principal: user("eve"),
                    write: true,
                },
            )
            .await
            .unwrap();
            let (set, remove) = plan_shares(&nc.shares(slug).await.unwrap(), &wanted);
            assert!(remove.contains(&user("eve")), "{remove:?}");
            for sh in &set {
                nc.share(slug, sh).await.unwrap();
            }
            for pr in &remove {
                nc.unshare(slug, pr).await.unwrap();
            }
            let mut now = nc.shares(slug).await.unwrap();
            now.sort();
            let mut want = wanted.clone();
            want.sort();
            assert_eq!(now, want, "{slug}");
            assert_eq!(
                plan_shares(&now, &wanted),
                (vec![], vec![]),
                "a second round changes nothing"
            );
        }

        // a kitchen member writes the series into it; a resident reads only; others see nothing
        let anna = shared_url(&dav, "anna", &kitchen)
            .await
            .expect("anna sees the kitchen's calendar");
        let (status, body) = request(
            "PUT",
            &format!("{anna}it-kitchen.ics"),
            "anna",
            SERIES,
            "text/calendar; charset=utf-8",
            None,
        )
        .await;
        assert!(
            status == 201 || status == 204,
            "anna writes: {status} {body}"
        );
        let ben = shared_url(&dav, "ben", &kitchen)
            .await
            .expect("ben (residents) sees it");
        let (status, _) = request(
            "PUT",
            &format!("{ben}ben.ics"),
            "ben",
            &SERIES.replace("it-kitchen", "ben"),
            "text/calendar",
            None,
        )
        .await;
        // refused: this Nextcloud answers 404 for a read-only share (checked 2026-10-22), 403 elsewhere
        assert!(
            status == 403 || status == 404,
            "ben may only read: {status}"
        );
        assert!(
            shared_url(&dav, "eve", &kitchen).await.is_none(),
            "eve sees nothing"
        );
        assert!(
            shared_url(&dav, "ben", &finance).await.is_none(),
            "the Finanz-AG's is private"
        );
        assert!(shared_url(&dav, "anna", &finance).await.is_none());

        // the bot reads it: the moved one keeps its ID, the excluded one is gone
        let from = NaiveDate::from_ymd_opt(2026, 10, 1).unwrap();
        let to = NaiveDate::from_ymd_opt(2027, 1, 31).unwrap();
        let occ = occurrences(&nc.events(&kitchen, from, to).await.unwrap(), from, to);
        let list: Vec<(String, String)> = occ
            .iter()
            .map(|o| (o.id.clone(), o.start.to_string()))
            .collect();
        assert_eq!(
            list,
            [
                ("it-kitchen#2026-11-02", "2026-11-02 20:00"),
                ("it-kitchen#2026-11-09", "2026-11-10 21:00"),
                ("it-kitchen#2026-11-23", "2026-11-23 20:00"),
                ("it-kitchen#2026-11-30", "2026-11-30 20:00"),
            ]
            .map(|(a, b)| (a.to_owned(), b.to_owned()))
        );
        // calendar-bot finds the kitchen's among its calendars, not the Finanz-AG's
        let src = caldav::Source {
            caldav_account_url: Some(format!("{dav}/calendars/calendar-bot/")),
            username: Some("calendar-bot".into()),
            password: Some(pass("calendar-bot")),
            ..Default::default()
        };
        let urls = caldav::discover_calendars(&reqwest::Client::new(), &src)
            .await
            .unwrap();
        assert!(urls.iter().any(|u| u.contains(&kitchen)), "{urls:?}");
        assert!(!urls.iter().any(|u| u.contains(&finance)), "{urls:?}");

        for slug in [&kitchen, &finance] {
            let (status, _) = request(
                "DELETE",
                &nc.calendar_url(slug),
                "plenum-bot",
                "",
                "text/plain",
                None,
            )
            .await;
            assert!(status == 204 || status == 200, "{slug}: {status}");
        }
    }
}
