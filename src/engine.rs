//! What happens when: every minute [`Engine::tick`] walks the plenums and their meetings,
//! and the chat commands land here too. Outline and Matrix are behind [`Wiki`] and [`Chat`],
//! so the whole flow is tested with fakes. The dates are the plenum's Nextcloud calendar
//! (`apply_calendar`, read by main.rs): a meeting is one of its occurrences, found again by
//! its ID, so moving it in the calendar moves the meeting with its protocol (renamed,
//! announced again), and one that leaves the calendar before it began is cancelled (said
//! once in the room; the protocol stays in the wiki). While the calendar cannot be read,
//! nothing changes.
//!
//! A meeting goes through
//!   prepare  (`prepare_days` before: the protocol from the template, chat items added)
//!   announce (the link in the plenum's room)
//!   remind   (`remind_minutes` before the start)
//!   close    (`close_after_hours` after the end, or `!protokoll fertig`: decisions into the
//!            decision list, the PDF attached and posted, the summary)
//! and each step is saved when done, so a restart neither repeats nor skips one. An error in
//! one meeting is logged and retried at the next tick; the other meetings go on. Everything
//! runs under one lock: a tick and a command never interleave (a command may wait for a slow
//! tick, at most the Outline and Matrix timeouts).
//!
//! Every plenum has its area in the wiki (`setup`): the collection with the access its config
//! asks for, the template, the decision list, the rules people keep, the document the
//! protocols are filed under, and an overview the bot writes anew whenever it changes (the
//! next dates, open tasks, the latest decisions). Open tasks of a closed protocol go into the
//! next one as a checklist. A plenum with a group also keeps its room's members in line with
//! it (`sync_members`).
//!
//! The bot never rewrites a protocol people write in: chat items go in with Outline's
//! `patch` (only the marker line is replaced), everything else is appended. A full rewrite
//! would go through Outline's Markdown, which does not survive every text (a literal `[x]`
//! comes back as a checkbox).

use std::{
    collections::{BTreeSet, HashSet},
    path::PathBuf,
};

use anyhow::{Context, Result};
use chrono::{DateTime, Duration, NaiveDate, Utc};
use chrono_tz::Tz;
use mxbot_common::matrix_sdk::ruma::{OwnedRoomId, OwnedUserId, RoomId, UserId};
use tokio::sync::Mutex;
use tracing::{error, info, warn};

use crate::{
    calendar::Occurrence,
    commands::Command,
    config::{Access, PdfConfig, Plenum},
    directory::Groups,
    members::{self, Member},
    protocol::{self, PdfNote},
    schedule::{self, MeetingKey},
    state::{Carried, Meeting, State, Top},
};

/// A document as the bot needs it.
#[derive(Clone, Debug, PartialEq)]
pub struct Doc {
    pub id: String,
    pub title: String,
    pub text: String,
    pub url: String,
    pub revision: i64,
    pub collection_id: Option<String>,
    pub parent_id: Option<String>,
}

/// The wiki (Outline in production).
#[allow(async_fn_in_trait)]
pub trait Wiki {
    /// The collection's ID; created if no collection has that name, and set to `access`
    /// (everyone writes; everyone reads; nobody but its groups) if it is not.
    async fn ensure_collection(&self, name: &str, access: Access) -> Result<String>;
    /// The wiki group `group` may write in the collection; false when the wiki has no such
    /// group (yet: outline-roles makes it from the directory).
    async fn grant_group(&self, collection_id: &str, group: &str) -> Result<bool>;
    /// A live document with exactly this title in the collection.
    async fn find_doc(&self, collection_id: &str, title: &str) -> Result<Option<Doc>>;
    async fn create_doc(
        &self,
        id: &str,
        collection_id: &str,
        parent_id: Option<&str>,
        title: &str,
        text: &str,
    ) -> Result<Doc>;
    /// File the document under `parent_id` in the collection.
    async fn move_doc(&self, id: &str, collection_id: &str, parent_id: &str) -> Result<()>;
    async fn rename(&self, id: &str, title: &str) -> Result<()>;
    /// Replace the whole text: only for the overview, which the bot alone writes.
    async fn replace(&self, id: &str, text: &str) -> Result<()>;
    /// None when it does not exist or is deleted (not when access is refused: that is an error).
    async fn doc(&self, id: &str) -> Result<Option<Doc>>;
    /// Replace the first `find` in the document with `text`, nothing else; false when `find`
    /// is not in it.
    async fn patch(&self, id: &str, find: &str, text: &str) -> Result<bool>;
    async fn append(&self, id: &str, text: &str) -> Result<()>;
    /// The attachment's URL path.
    async fn attach(
        &self,
        doc_id: &str,
        name: &str,
        content_type: &str,
        bytes: Vec<u8>,
    ) -> Result<String>;
    /// A URL path as a link people can open.
    fn link(&self, path: &str) -> String;
}

/// The chat (Matrix in production).
#[allow(async_fn_in_trait)]
pub trait Chat {
    /// Send Markdown with this transaction ID (a resend with the same ID is dropped).
    async fn send(&self, room: &RoomId, markdown: &str, txn: &str) -> Result<()>;
    async fn send_file(&self, room: &RoomId, name: &str, bytes: Vec<u8>, txn: &str) -> Result<()>;
    /// Who is in the room, joined or invited.
    async fn members(&self, room: &RoomId) -> Result<Vec<Member>>;
    async fn invite(&self, room: &RoomId, user: &UserId) -> Result<()>;
    /// Remove someone (also an open invitation), with the reason they see.
    async fn remove(&self, room: &RoomId, user: &UserId, reason: &str) -> Result<()>;
}

/// Where a command comes from: what the overview may show there.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Scope {
    /// A plenum's room, or the bot's own room: private plenums only in their own room.
    Room,
    /// An admin's private chat with the bot: everything.
    AdminDm,
}

/// How often the area of a plenum (collection access, documents, filing) is checked.
const AREA_EVERY: Duration = Duration::hours(1);

/// Dates the overview lists.
const OVERVIEW_DATES: usize = 4;

/// Failed attempts at the PDF (and, separately, at posting it) before it is given up; the
/// summary then goes out without it and says so.
const PDF_ATTEMPTS: u32 = 3;

pub struct Engine<W, C> {
    pub plenums: Vec<Plenum>,
    pub tz: Tz,
    pub wiki: W,
    pub chat: C,
    pub pdf: PdfConfig,
    pub admins: HashSet<OwnedUserId>,
    /// The bot's own account (never removed from a room); None: rooms are not managed.
    pub me: Option<OwnedUserId>,
    /// `[members] keep`: never removed from a room.
    pub keep: HashSet<OwnedUserId>,
    /// Where people open the calendars (Nextcloud's calendar app), for the links.
    pub calendar_url: Option<String>,
    pub(crate) state: Mutex<State>,
    state_path: PathBuf,
}

/// Who sent a command.
pub struct Sender {
    pub id: OwnedUserId,
    pub name: String,
}

/// The meeting `id` of plenum `p` in the state (it must exist).
fn meeting<'a>(st: &'a mut State, p: &Plenum, id: &str) -> &'a mut Meeting {
    st.plenums
        .get_mut(&p.key)
        .and_then(|ps| ps.meetings.get_mut(id))
        .expect("the meeting is in the state")
}

impl<W: Wiki, C: Chat> Engine<W, C> {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        plenums: Vec<Plenum>,
        tz: Tz,
        wiki: W,
        chat: C,
        pdf: PdfConfig,
        admins: HashSet<OwnedUserId>,
        state: State,
        state_path: PathBuf,
    ) -> Self {
        Self {
            plenums,
            tz,
            wiki,
            chat,
            pdf,
            admins,
            me: None,
            keep: HashSet::new(),
            calendar_url: None,
            state: Mutex::new(state),
            state_path,
        }
    }

    pub fn rooms(&self) -> HashSet<OwnedRoomId> {
        self.plenums.iter().map(|p| p.room.clone()).collect()
    }

    fn today(&self, now: DateTime<Utc>) -> NaiveDate {
        now.with_timezone(&self.tz).date_naive()
    }

    async fn save(&self, st: &State) {
        if let Err(e) = st.save(&self.state_path).await {
            error!("Saving the state failed: {e:#}");
        }
    }

    /// Everything that is due. Errors are logged per meeting; the others go on.
    pub async fn tick(&self, now: DateTime<Utc>) {
        let mut st = self.state.lock().await;
        for p in &self.plenums {
            self.follow_calendar(&mut st, p, now).await;
            self.tick_plenum(&mut st, p, now).await;
            if let Err(e) = self.tick_area(&mut st, p, now).await {
                warn!(plenum = %p.key, "Wiki area: {e:#}");
            }
            let today = self.today(now);
            let _ = today;
            st.plenums.entry(p.key.clone()).or_default().prune();
        }
        self.save(&st).await;
    }

    /// The area in the wiki: checked every [`AREA_EVERY`] (and right after the start), the
    /// protocols filed, the overview written when its text changed.
    async fn tick_area(&self, st: &mut State, p: &Plenum, now: DateTime<Utc>) -> Result<()> {
        let due = st
            .plenums
            .get(&p.key)
            .and_then(|ps| ps.area_checked)
            .is_none_or(|at| now - at >= AREA_EVERY);
        if due {
            self.setup(st, p).await?;
            self.file_protocols(st, p).await;
            st.plenums.entry(p.key.clone()).or_default().area_checked = Some(now);
            self.save(st).await;
        }
        self.write_overview(st, p, now).await
    }

    /// Protocols made before the protocols document existed (at the top of the collection)
    /// go under it, once each.
    async fn file_protocols(&self, st: &mut State, p: &Plenum) {
        let ps = st.plenums.entry(p.key.clone()).or_default();
        let (Some(collection), Some(parent)) =
            (ps.collection_id.clone(), ps.protocols_doc_id.clone())
        else {
            return;
        };
        let todo: Vec<(String, String)> = ps
            .meetings
            .iter()
            .filter(|(_, m)| m.created && !m.filed)
            .map(|(k, m)| (k.clone(), m.doc_id.clone()))
            .collect();
        for (key, doc_id) in todo {
            let filed = match self.wiki.doc(&doc_id).await {
                Ok(Some(d)) if d.parent_id.as_deref() == Some(parent.as_str()) => true,
                Ok(Some(d)) if d.collection_id.as_deref() == Some(collection.as_str()) => {
                    match self.wiki.move_doc(&doc_id, &collection, &parent).await {
                        Ok(()) => {
                            info!(plenum = %p.key, meeting = %key, "Protocol filed under the protocols");
                            true
                        }
                        Err(e) => {
                            warn!(plenum = %p.key, meeting = %key, "Filing the protocol: {e:#}");
                            false
                        }
                    }
                }
                // gone, or moved elsewhere by someone: left where it is
                Ok(_) => true,
                Err(e) => {
                    warn!(plenum = %p.key, meeting = %key, "Filing the protocol: {e:#}");
                    false
                }
            };
            if filed {
                if let Some(m) = st
                    .plenums
                    .get_mut(&p.key)
                    .and_then(|ps| ps.meetings.get_mut(&key))
                {
                    m.filed = true;
                }
            }
        }
    }

    /// The overview's text now (None until the area is set up).
    fn overview(&self, st: &mut State, p: &Plenum, now: DateTime<Utc>) -> Option<String> {
        let next = self.upcoming(st, p, now, OVERVIEW_DATES);
        let ps = st.plenums.get(&p.key)?;
        let url = |role: &str| ps.urls.get(role).map(|u| self.wiki.link(u));
        let (Some(rules), Some(decisions), Some(protocols), Some(template)) = (
            url("rules"),
            url("decisions"),
            url("protocols"),
            url("template"),
        ) else {
            return None;
        };
        let next: Vec<(MeetingKey, Option<String>)> = next
            .into_iter()
            .map(|(id, start)| {
                let link = ps
                    .meetings
                    .get(&id)
                    .filter(|m| m.created)
                    .map(|m| self.link_of(m));
                (start, link)
            })
            .collect();
        let links = protocol::OverviewLinks {
            rules: &rules,
            decisions: &decisions,
            protocols: &protocols,
            template: &template,
            calendar: self.calendar_url.as_deref(),
        };
        Some(protocol::overview_text(
            p,
            &next,
            &ps.open_tasks,
            &ps.decisions,
            &links,
        ))
    }

    async fn write_overview(&self, st: &mut State, p: &Plenum, now: DateTime<Utc>) -> Result<()> {
        let Some(text) = self.overview(st, p, now) else {
            return Ok(());
        };
        let ps = st.plenums.entry(p.key.clone()).or_default();
        if ps.overview_text.as_deref() == Some(text.as_str()) {
            return Ok(());
        }
        let Some(id) = ps.overview_doc_id.clone() else {
            return Ok(());
        };
        self.wiki.replace(&id, &text).await?;
        let ps = st.plenums.entry(p.key.clone()).or_default();
        ps.overview_text = Some(text);
        self.save(st).await;
        Ok(())
    }

    /// The calendar as last read: an occurrence a meeting is known by keeps it (taking over
    /// one from before the calendar with the same start), a moved one is renamed and
    /// announced again, one gone from the calendar before it began is cancelled, one that
    /// comes back is not any more.
    async fn follow_calendar(&self, st: &mut State, p: &Plenum, now: DateTime<Utc>) {
        let ps = st.plenums.entry(p.key.clone()).or_default();
        if ps.calendar_read.is_none() {
            return;
        }
        let occurrences = ps.occurrences.clone();
        let ids: HashSet<&str> = occurrences.iter().map(|o| o.id.as_str()).collect();
        let mut moved = Vec::new();
        for occ in &occurrences {
            if !ps.meetings.contains_key(&occ.id) {
                let legacy = occ.start.to_string();
                if ps.meetings.get(&legacy).is_some_and(|m| m.start.is_none()) {
                    let m = ps.meetings.remove(&legacy).expect("just looked at");
                    info!(plenum = %p.key, meeting = %occ.id, "A meeting from before the calendar, taken over by its occurrence");
                    ps.meetings.insert(occ.id.clone(), m);
                }
            }
            let Some(m) = ps.meetings.get_mut(&occ.id) else {
                continue;
            };
            if m.cancelled && !m.close.done {
                info!(plenum = %p.key, meeting = %occ.id, "Back in the calendar");
                m.cancelled = false;
                m.skipped = false;
                m.cancel_notice = Default::default();
            }
            if let Some(old) = m.start {
                if old != occ.start && m.created && !m.close.done {
                    moved.push((occ.id.clone(), old));
                }
            }
            m.start = Some(occ.start);
            m.end = occ.end;
        }
        let tz = self.tz;
        let gone: Vec<String> = ps
            .meetings
            .iter()
            .filter(|(id, m)| {
                !ids.contains(id.as_str())
                    && m.created
                    && !m.close.done
                    && !m.cancelled
                    && !m.skipped
                    && m.start_of(id).is_some_and(|s| s.start(tz) > now)
            })
            .map(|(id, _)| id.clone())
            .collect();
        // occurrences not prepared yet that left the calendar: nothing to keep
        ps.meetings
            .retain(|id, m| m.created || m.close.done || m.skipped || ids.contains(id.as_str()));
        for (id, old) in moved {
            if let Err(e) = self.moved(st, p, &id, old, now).await {
                warn!(plenum = %p.key, meeting = %id, "Following a move: {e:#}");
            }
        }
        for id in gone {
            if let Err(e) = self.cancelled(st, p, &id).await {
                warn!(plenum = %p.key, meeting = %id, "Following a cancellation: {e:#}");
            }
        }
    }

    /// Moved in the calendar: the protocol's title follows, and a meeting still to come is
    /// announced and reminded of again (the announcement says from when).
    async fn moved(
        &self,
        st: &mut State,
        p: &Plenum,
        id: &str,
        old: MeetingKey,
        now: DateTime<Utc>,
    ) -> Result<()> {
        let new = meeting(st, p, id)
            .start_of(id)
            .expect("set from the calendar");
        info!(plenum = %p.key, meeting = %id, from = %old, to = %new, "Moved in the calendar");
        let title = self.free_title(st, p, id, new);
        let doc_id = meeting(st, p, id).doc_id.clone();
        if meeting(st, p, id).title.as_deref() != Some(title.as_str()) {
            self.wiki.rename(&doc_id, &title).await?;
            meeting(st, p, id).title = Some(title);
        }
        if new.start(self.tz) > now {
            let m = meeting(st, p, id);
            if m.announce.done {
                m.moved_from = Some(m.moved_from.unwrap_or(old));
            }
            m.announce = Default::default();
            m.remind = Default::default();
        }
        self.save(st).await;
        Ok(())
    }

    /// Gone from the calendar before it began: said once in the room if it was announced.
    async fn cancelled(&self, st: &mut State, p: &Plenum, id: &str) -> Result<()> {
        let m = meeting(st, p, id);
        m.cancelled = true;
        info!(plenum = %p.key, meeting = %id, "Gone from the calendar: cancelled");
        if m.announce.done && !m.cancel_notice.done {
            let txn = m.cancel_notice.txn();
            let start = m.start_of(id).expect("a known meeting");
            let link = self.link_of(m);
            self.save(st).await;
            self.chat
                .send(&p.room, &protocol::cancellation(p, start, &link), &txn)
                .await?;
            meeting(st, p, id).cancel_notice.done = true;
        }
        self.save(st).await;
        Ok(())
    }

    /// The calendar's occurrences of the plenum, as just read (main.rs, every few minutes).
    pub async fn apply_calendar(
        &self,
        key: &str,
        occurrences: Vec<Occurrence>,
        now: DateTime<Utc>,
    ) {
        let mut st = self.state.lock().await;
        let ps = st.plenums.entry(key.to_owned()).or_default();
        ps.occurrences = occurrences;
        ps.calendar_read = Some(now);
        self.save(&st).await;
    }

    async fn tick_plenum(&self, st: &mut State, p: &Plenum, now: DateTime<Utc>) {
        let today = self.today(now);
        let from = today
            - Duration::days(
                i64::from(p.duration_minutes / 1440 + 2) + i64::from(p.close_after_hours / 24 + 1),
            );
        let to = today + Duration::days(i64::from(p.prepare_days) + 1);
        let ps = st.plenums.entry(p.key.clone()).or_default();
        // The calendar's occurrences in view get their meeting.
        for occ in ps.occurrences.clone() {
            if occ.start.date() >= from && occ.start.date() <= to {
                let m = ps
                    .meetings
                    .entry(occ.id.clone())
                    .or_insert_with(|| Meeting {
                        doc_id: uuid::Uuid::new_v4().to_string(),
                        ..Default::default()
                    });
                m.start = Some(occ.start);
                m.end = occ.end;
            }
        }
        // ... and so do the open protocols, also one whose meeting left the calendar after it
        // began (it is closed as usual).
        let mut due: Vec<(MeetingKey, String)> = ps
            .meetings
            .iter()
            .filter(|(_, m)| !m.close.done && !m.skipped && !m.cancelled)
            .filter_map(|(id, m)| {
                let start = m.start_of(id)?;
                (m.created || (start.date() >= from && start.date() <= to))
                    .then(|| (start, id.clone()))
            })
            .collect();
        due.sort();
        for (_, id) in due {
            if let Err(e) = self.tick_meeting(st, p, &id, now).await {
                warn!(plenum = %p.key, meeting = %id, "{e:#}");
            }
        }
    }

    /// When the meeting ends: the calendar's end, else its start and `duration_minutes`.
    fn end_of(&self, p: &Plenum, id: &str, m: &Meeting) -> DateTime<Utc> {
        let start = m.start_of(id).map(|s| s.start(self.tz)).unwrap_or_default();
        match m.end {
            Some(end) if MeetingKey(end).start(self.tz) > start => MeetingKey(end).start(self.tz),
            _ => start + Duration::minutes(i64::from(p.duration_minutes)),
        }
    }

    /// The protocol's title for a meeting starting at `start`: "<Name> YYYY-MM-DD", with the
    /// time when another meeting that day has that title already.
    fn free_title(&self, st: &State, p: &Plenum, id: &str, start: MeetingKey) -> String {
        let title = protocol::title(p, start);
        let taken = st.plenums.get(&p.key).is_some_and(|ps| {
            ps.meetings.iter().any(|(other, m)| {
                other != id && m.created && m.title.as_deref() == Some(title.as_str())
            })
        });
        if taken {
            format!("{title} {}", start.0.format("%H:%M"))
        } else {
            title
        }
    }

    async fn tick_meeting(
        &self,
        st: &mut State,
        p: &Plenum,
        key: &str,
        now: DateTime<Utc>,
    ) -> Result<()> {
        let (start_key, end) = {
            let m = &st.plenums[&p.key].meetings[key];
            (
                m.start_of(key).context("a meeting without a start")?,
                self.end_of(p, key, m),
            )
        };
        let start = start_key.start(self.tz);
        // In local days, so a DST change in between moves nothing by an hour.
        let prepare_at =
            MeetingKey(start_key.0 - Duration::days(i64::from(p.prepare_days))).start(self.tz);
        {
            let ps = st.plenums.entry(p.key.clone()).or_default();
            let m = ps.meetings.get_mut(key).expect("due");
            if m.close.done || m.skipped || m.cancelled {
                return Ok(());
            }
            if now < prepare_at {
                return Ok(());
            }
            if !m.created && now >= end {
                // The bot was down for the whole meeting: there is no protocol to close.
                info!(plenum = %p.key, meeting = %key, "Ended before it was prepared, skipped");
                m.skipped = true;
                return Ok(());
            }
        }
        self.prepare(st, p, key).await?;
        self.announce(st, p, key).await?;
        if p.remind_minutes > 0 {
            let remind_at = start - Duration::minutes(i64::from(p.remind_minutes));
            if now >= remind_at && now < end {
                self.remind(st, p, key, now >= start).await?;
            }
        }
        let requested = st.plenums[&p.key].meetings[key].close.requested;
        let close_at = end + Duration::hours(i64::from(p.close_after_hours));
        if requested || now >= close_at {
            self.close(st, p, key, now).await?;
        }
        Ok(())
    }

    /// The plenum's area, found or created again (also when someone deleted a part): the
    /// collection with its access and group, the template, the decision list, the rules, the
    /// protocols document and the overview. `(collection, template text, decisions doc)`.
    async fn setup(&self, st: &mut State, p: &Plenum) -> Result<(String, String, Doc)> {
        // Looked up every time (a protocol is prepared rarely): a cached ID of a deleted
        // collection would fail forever.
        let collection = self.wiki.ensure_collection(&p.collection, p.access).await?;
        if let (Some(group), true) = (&p.group, p.access != Access::House) {
            if !self.wiki.grant_group(&collection, group).await? {
                warn!(plenum = %p.key, %group, "The wiki has no such group yet (outline-roles makes it): only its admins write here for now");
            }
        }
        let ps = st.plenums.entry(p.key.clone()).or_default();
        ps.collection_id = Some(collection.clone());
        let known = (
            ps.rules_doc_id.clone(),
            ps.protocols_doc_id.clone(),
            ps.overview_doc_id.clone(),
        );
        let rules = self
            .ensure_doc(
                &collection,
                &p.rules_title(),
                &protocol::rules_doc_text(p),
                known.0.as_deref(),
            )
            .await?;
        let protocols = self
            .ensure_doc(
                &collection,
                &p.protocols_title(),
                &protocol::protocols_doc_text(p),
                known.1.as_deref(),
            )
            .await?;
        let overview = self
            .ensure_doc(&collection, &p.overview_title(), "", known.2.as_deref())
            .await?;
        let ps = st.plenums.entry(p.key.clone()).or_default();
        if ps.overview_doc_id.as_deref() != Some(overview.id.as_str()) {
            ps.overview_text = None; // a new overview document: written again
        }
        ps.urls.insert("rules".into(), rules.url);
        ps.urls.insert("protocols".into(), protocols.url);
        ps.urls.insert("overview".into(), overview.url);
        ps.rules_doc_id = Some(rules.id);
        ps.protocols_doc_id = Some(protocols.id);
        ps.overview_doc_id = Some(overview.id);
        let template = self
            .ensure_doc(
                &collection,
                &p.template_title(),
                protocol::DEFAULT_TEMPLATE,
                ps.template_doc_id.as_deref(),
            )
            .await?;
        ps.template_doc_id = Some(template.id.clone());
        ps.urls.insert("template".into(), template.url.clone());
        let decisions = self
            .ensure_doc(
                &collection,
                &p.decisions_title(),
                &protocol::decisions_doc_text(p),
                ps.decisions_doc_id.as_deref(),
            )
            .await?;
        ps.decisions_doc_id = Some(decisions.id.clone());
        ps.urls.insert("decisions".into(), decisions.url.clone());
        Ok((collection, template.text, decisions))
    }

    /// The document `known` (if it still exists and is in the collection), else the one with
    /// `title`, else a new one.
    async fn ensure_doc(
        &self,
        collection: &str,
        title: &str,
        text: &str,
        known: Option<&str>,
    ) -> Result<Doc> {
        if let Some(id) = known {
            if let Some(d) = self.wiki.doc(id).await? {
                if d.collection_id.as_deref().is_none_or(|c| c == collection) {
                    return Ok(d);
                }
            }
        }
        if let Some(d) = self.wiki.find_doc(collection, title).await? {
            return Ok(d);
        }
        info!(%title, "Creating the document");
        self.wiki
            .create_doc(
                &uuid::Uuid::new_v4().to_string(),
                collection,
                None,
                title,
                text,
            )
            .await
    }

    async fn prepare(&self, st: &mut State, p: &Plenum, key: &str) -> Result<()> {
        if st.plenums[&p.key]
            .meetings
            .get(key)
            .is_some_and(|m| m.created)
        {
            return Ok(());
        }
        let (collection, template, _) = self.setup(st, p).await?;
        let start = st.plenums[&p.key].meetings[key]
            .start_of(key)
            .context("a meeting without a start")?;
        let title = match st.plenums[&p.key].meetings[key].title.clone() {
            Some(t) => t,
            None => self.free_title(st, p, key, start),
        };
        let ps = st.plenums.get_mut(&p.key).expect("set up");
        let m = ps.meetings.get_mut(key).expect("due");
        m.title = Some(title.clone());
        let doc_id = m.doc_id.clone();
        // Saved before the call: after a crash the same ID is looked up instead of a second
        // protocol being created.
        self.save(st).await;
        let pending: Vec<String> = st.plenums[&p.key]
            .pending_tops
            .iter()
            .map(protocol::top_line)
            .collect();
        let doc = match self.wiki.doc(&doc_id).await? {
            // Created before a restart: items sent since then go in like any chat item.
            Some(d) => {
                if self.add_lines(&doc_id, &pending).await?.is_none() {
                    anyhow::bail!("the protocol vanished while it was being prepared");
                }
                d
            }
            None => {
                let ps = &st.plenums[&p.key];
                let mut text = protocol::render_template(&template, p, start, &ps.open_tasks);
                for line in &pending {
                    text = protocol::insert_top(&text, line);
                }
                let parent = ps.protocols_doc_id.clone();
                info!(plenum = %p.key, meeting = %key, "Creating the protocol");
                self.wiki
                    .create_doc(&doc_id, &collection, parent.as_deref(), &title, &text)
                    .await?
            }
        };
        let ps = st.plenums.get_mut(&p.key).expect("set up");
        ps.pending_tops.clear();
        let filed = ps.protocols_doc_id.is_some() && doc.parent_id == ps.protocols_doc_id;
        let m = ps.meetings.get_mut(key).expect("inserted");
        m.created = true;
        m.filed = filed;
        m.url = Some(doc.url);
        info!(plenum = %p.key, meeting = %key, tops = pending.len(), "Protocol ready");
        self.save(st).await;
        Ok(())
    }

    /// Agenda lines into a protocol people may be writing in: each in place of the marker
    /// (Outline's patch touches nothing else), without a marker appended under
    /// [`protocol::CHAT_ITEMS_HEADING`]. Lines already in it are left out. None: the
    /// protocol is gone.
    async fn add_lines(&self, doc_id: &str, lines: &[String]) -> Result<Option<()>> {
        let Some(doc) = self.wiki.doc(doc_id).await? else {
            return Ok(None);
        };
        let mut heading = protocol::contains_line(&doc.text, protocol::CHAT_ITEMS_HEADING);
        for line in lines {
            if protocol::contains_line(&doc.text, line) {
                continue;
            }
            if doc.text.contains(protocol::TOP_MARKER)
                && self
                    .wiki
                    .patch(
                        doc_id,
                        protocol::TOP_MARKER,
                        &format!("{line}\n\n{}", protocol::TOP_MARKER),
                    )
                    .await?
            {
                continue;
            }
            let text = if heading {
                format!("\n\n{line}")
            } else {
                format!("\n\n{}\n\n{line}", protocol::CHAT_ITEMS_HEADING)
            };
            self.wiki.append(doc_id, &text).await?;
            heading = true;
        }
        Ok(Some(()))
    }

    fn link_of(&self, m: &Meeting) -> String {
        m.url
            .as_deref()
            .map(|u| self.wiki.link(u))
            .unwrap_or_default()
    }

    async fn announce(&self, st: &mut State, p: &Plenum, key: &str) -> Result<()> {
        let m = meeting(st, p, key);
        if m.announce.done {
            return Ok(());
        }
        let txn = m.announce.txn();
        let link = self.link_of(m);
        let doc_id = m.doc_id.clone();
        let start = m.start_of(key).context("a meeting without a start")?;
        let moved_from = m.moved_from;
        self.save(st).await;
        // The number of items is only known in the document now; count the list lines.
        let tops = self
            .wiki
            .doc(&doc_id)
            .await?
            .map(|d| count_tops(&d.text))
            .unwrap_or(0);
        self.chat
            .send(
                &p.room,
                &protocol::announcement(p, start, &link, tops, moved_from),
                &txn,
            )
            .await?;
        meeting(st, p, key).announce.done = true;
        self.save(st).await;
        Ok(())
    }

    async fn remind(&self, st: &mut State, p: &Plenum, key: &str, started: bool) -> Result<()> {
        let m = meeting(st, p, key);
        if m.remind.done {
            return Ok(());
        }
        let txn = m.remind.txn();
        let link = self.link_of(m);
        let start = m.start_of(key).context("a meeting without a start")?;
        self.save(st).await;
        self.chat
            .send(&p.room, &protocol::reminder(p, start, &link, started), &txn)
            .await?;
        meeting(st, p, key).remind.done = true;
        self.save(st).await;
        Ok(())
    }

    async fn close(&self, st: &mut State, p: &Plenum, key: &str, now: DateTime<Utc>) -> Result<()> {
        let (doc_id, link, start, title) = {
            let m = meeting(st, p, key);
            let start = m.start_of(key).context("a meeting without a start")?;
            let title = m.title.clone().unwrap_or_else(|| protocol::title(p, start));
            (m.doc_id.clone(), self.link_of(m), start, title)
        };
        let Some(doc) = self.wiki.doc(&doc_id).await? else {
            warn!(plenum = %p.key, meeting = %key, "The protocol was deleted; closed without it");
            meeting(st, p, key).close.done = true;
            self.save(st).await;
            return Ok(());
        };
        let outcome = protocol::extract(&doc.text);
        // What the next protocol and the overview carry on with (each close sets it anew,
        // so a repeated close changes nothing).
        {
            let ps = st.plenums.entry(p.key.clone()).or_default();
            let date = start.date();
            let carried = |text: &String| Carried {
                text: text.clone(),
                date,
                link: link.clone(),
            };
            ps.open_tasks = outcome.tasks.iter().map(carried).collect();
            ps.decisions.retain(|d| d.date != date);
            ps.decisions.extend(outcome.decisions.iter().map(carried));
            ps.decisions.sort_by_key(|d| d.date);
        }

        // 1. the decisions into the plenum's list (found or created again)
        let mut decisions_link = None;
        if !outcome.decisions.is_empty() {
            let (_, _, decisions) = self.setup(st, p).await?;
            decisions_link = Some(self.wiki.link(&decisions.url));
            if !meeting(st, p, key).close.decisions_saved {
                // After a crash between the append and the save, the block is there already.
                if !decisions.text.contains(&link) {
                    let block = protocol::decisions_block(&title, &link, &outcome.decisions);
                    self.wiki
                        .append(&decisions.id, &format!("\n\n{block}"))
                        .await?;
                }
                meeting(st, p, key).close.decisions_saved = true;
                self.save(st).await;
            }
        }

        // 2. the PDF, attached to the protocol and linked in it, then posted in the room
        if self.pdf.enabled && !meeting(st, p, key).close.pdf_failed {
            if let Err(e) = self.attach_pdf(st, p, key, &doc, &title, now).await {
                let c = &mut meeting(st, p, key).close;
                c.pdf_attempts += 1;
                if c.pdf_attempts < PDF_ATTEMPTS {
                    self.save(st).await;
                    return Err(e.context("PDF"));
                }
                error!(plenum = %p.key, meeting = %key, "PDF given up after {} attempts: {e:#}", c.pdf_attempts);
                c.pdf_failed = true;
                self.save(st).await;
            }
        }
        let c = &meeting(st, p, key).close;
        if self.pdf.post_to_room && c.pdf_url.is_some() && !c.pdf_room.done && !c.pdf_room_failed {
            if let Err(e) = self.post_pdf(st, p, key, &doc, &title, now).await {
                let c = &mut meeting(st, p, key).close;
                c.pdf_room_attempts += 1;
                if c.pdf_room_attempts < PDF_ATTEMPTS {
                    self.save(st).await;
                    return Err(e.context("PDF into the room"));
                }
                error!(plenum = %p.key, meeting = %key, "Posting the PDF given up: {e:#}");
                c.pdf_room_failed = true;
                self.save(st).await;
            }
        }

        // 3. the summary
        let m = meeting(st, p, key);
        if !m.close.summary.done {
            let txn = m.close.summary.txn();
            let note = if m.close.pdf_failed {
                PdfNote::Failed
            } else if m.close.pdf_room_failed {
                PdfNote::OnlyInWiki
            } else {
                PdfNote::Fine
            };
            self.save(st).await;
            let text =
                protocol::summary(p, start, &link, &outcome, decisions_link.as_deref(), note);
            self.chat.send(&p.room, &text, &txn).await?;
            meeting(st, p, key).close.summary.done = true;
        }
        meeting(st, p, key).close.done = true;
        info!(plenum = %p.key, meeting = %key, decisions = outcome.decisions.len(), "Protocol closed");
        self.save(st).await;
        Ok(())
    }

    fn render_pdf(&self, doc: &Doc, title: &str, now: DateTime<Utc>) -> Result<Vec<u8>> {
        let stand = format!(
            "Protokoll aus dem Wiki, Stand {}",
            now.with_timezone(&self.tz).format("%d.%m.%Y %H:%M")
        );
        crate::pdf::render(
            title,
            &stand,
            &doc.text,
            std::path::Path::new(&self.pdf.font_dir),
        )
    }

    /// The PDF attached to the protocol and a link to it at the end (each once).
    async fn attach_pdf(
        &self,
        st: &mut State,
        p: &Plenum,
        key: &str,
        doc: &Doc,
        title: &str,
        now: DateTime<Utc>,
    ) -> Result<()> {
        let file = format!("{title}.pdf");
        if meeting(st, p, key).close.pdf_url.is_none() {
            let pdf = self.render_pdf(doc, title, now)?;
            let url = self
                .wiki
                .attach(&doc.id, &file, "application/pdf", pdf)
                .await?;
            meeting(st, p, key).close.pdf_url = Some(url);
            self.save(st).await;
        }
        let c = &meeting(st, p, key).close;
        if !c.pdf_linked {
            let url = c.pdf_url.clone().expect("attached");
            if !doc.text.contains(&url) {
                // A paragraph with an attachment link becomes a file card in Outline, which
                // drops any text before the link: the label is a paragraph of its own.
                self.wiki
                    .append(
                        &doc.id,
                        &format!("\n\n---\n\nDas Protokoll als PDF:\n\n[{file}]({url})"),
                    )
                    .await?;
            }
            meeting(st, p, key).close.pdf_linked = true;
            self.save(st).await;
        }
        Ok(())
    }

    /// The PDF as a file in the plenum's room (once).
    async fn post_pdf(
        &self,
        st: &mut State,
        p: &Plenum,
        key: &str,
        doc: &Doc,
        title: &str,
        now: DateTime<Utc>,
    ) -> Result<()> {
        let txn = meeting(st, p, key).close.pdf_room.txn();
        self.save(st).await;
        let pdf = self.render_pdf(doc, title, now)?;
        self.chat
            .send_file(&p.room, &format!("{title}.pdf"), pdf, &txn)
            .await?;
        meeting(st, p, key).close.pdf_room.done = true;
        self.save(st).await;
        Ok(())
    }

    // ── Commands ────────────────────────────────────────────────────────────────

    fn plenums_in(&self, room: &RoomId) -> Vec<&Plenum> {
        self.plenums.iter().filter(|p| p.room == room).collect()
    }

    /// The plenum a command is for: the named one, or the only one of the room.
    fn pick<'a>(&'a self, room: &RoomId, key: Option<&str>) -> Result<&'a Plenum, String> {
        let here = self.plenums_in(room);
        match key {
            Some(k) => here
                .into_iter()
                .find(|p| p.key == k)
                .ok_or_else(|| format!("Hier gibt es kein Plenum „{k}“. {}", self.keys_hint(room))),
            None => match here.as_slice() {
                [one] => Ok(one),
                [] => Err("In diesem Raum gibt es kein Plenum.".to_owned()),
                _ => Err(format!(
                    "Hier gibt es mehrere Plena, bitte dazuschreiben welches. {}",
                    self.keys_hint(room)
                )),
            },
        }
    }

    fn keys_hint(&self, room: &RoomId) -> String {
        let keys: Vec<String> = self
            .plenums_in(room)
            .iter()
            .map(|p| format!("`{}` ({})", p.key, p.name))
            .collect();
        if keys.is_empty() {
            String::new()
        } else {
            format!("Plena hier: {}", keys.join(", "))
        }
    }

    /// Dates and closing: the organizers (everyone in the room when there are none), admins.
    fn may_organize(&self, p: &Plenum, user: &UserId) -> bool {
        p.organizers.is_empty() || p.organizers.contains(user) || self.admins.contains(user)
    }

    /// A command from a room; the answer for the room.
    #[cfg_attr(not(test), allow(dead_code))]
    pub async fn command(
        &self,
        room: &RoomId,
        sender: &Sender,
        cmd: Command,
        now: DateTime<Utc>,
    ) -> String {
        self.command_in(room, sender, cmd, now, Scope::Room).await
    }

    /// A command from `room` (`scope`: an admin's private chat sees every plenum).
    pub async fn command_in(
        &self,
        room: &RoomId,
        sender: &Sender,
        cmd: Command,
        now: DateTime<Utc>,
        scope: Scope,
    ) -> String {
        const ORGANIZERS_ONLY: &str = "⛔ Das dürfen hier nur die Organisator*innen.";
        let mut st = self.state.lock().await;
        let answer = match cmd {
            Command::Help => help(),
            Command::Overview(key) => self.list(&mut st, room, key.as_deref(), now, scope),
            Command::Top { key, text } => match self.pick(room, key.as_deref()) {
                Ok(p) => self.add_top(&mut st, p, sender, &text, now).await,
                Err(e) => e,
            },
            Command::Close(key) => match self.pick(room, key.as_deref()) {
                Ok(p) if !self.may_organize(p, &sender.id) => ORGANIZERS_ONLY.to_owned(),
                Ok(p) => self.request_close(&mut st, p, now),
                Err(e) => e,
            },
            Command::Invalid(why) => why,
        };
        self.save(&st).await;
        drop(st);
        answer
    }

    /// The next meeting that is not closed yet, from now on (one that runs counts).
    fn next_meeting(
        &self,
        st: &mut State,
        p: &Plenum,
        now: DateTime<Utc>,
    ) -> Option<(String, MeetingKey)> {
        self.upcoming(st, p, now, 1).into_iter().next()
    }

    /// The meeting `!protokoll fertig` means: the latest one that has started and is open.
    fn current_meeting(&self, st: &State, p: &Plenum, now: DateTime<Utc>) -> Option<String> {
        let ps = st.plenums.get(&p.key)?;
        ps.meetings
            .iter()
            .filter(|(_, m)| m.created && !m.close.done && !m.skipped && !m.cancelled)
            .filter_map(|(id, m)| Some((m.start_of(id)?, id)))
            .filter(|(start, _)| start.start(self.tz) <= now)
            .max()
            .map(|(_, id)| id.clone())
    }

    /// The next meetings that are not closed yet, from now on (`n` at most): the calendar's,
    /// and open protocols.
    fn upcoming(
        &self,
        st: &mut State,
        p: &Plenum,
        now: DateTime<Utc>,
        n: usize,
    ) -> Vec<(String, MeetingKey)> {
        let ps = st.plenums.entry(p.key.clone()).or_default();
        let mut all: std::collections::BTreeMap<String, Meeting> = ps
            .occurrences
            .iter()
            .map(|o| {
                let m = ps.meetings.get(&o.id).cloned().unwrap_or_default();
                (
                    o.id.clone(),
                    Meeting {
                        start: Some(o.start),
                        end: o.end,
                        ..m
                    },
                )
            })
            .collect();
        for (id, m) in &ps.meetings {
            if m.created {
                all.entry(id.clone()).or_insert_with(|| m.clone());
            }
        }
        let mut list: Vec<(MeetingKey, String)> = all
            .iter()
            .filter(|(_, m)| !m.close.done && !m.skipped && !m.cancelled)
            .filter(|(id, m)| self.end_of(p, id, m) > now)
            .filter_map(|(id, m)| Some((m.start_of(id)?, id.clone())))
            .collect();
        list.sort();
        list.truncate(n);
        list.into_iter().map(|(start, id)| (id, start)).collect()
    }

    fn list(
        &self,
        st: &mut State,
        room: &RoomId,
        key: Option<&str>,
        now: DateTime<Utc>,
        scope: Scope,
    ) -> String {
        // A private plenum shows only in its own room (and to admins in private).
        let visible =
            |p: &&Plenum| p.access != Access::Private || p.room == room || scope == Scope::AdminDm;
        let list: Vec<&Plenum> = match key {
            Some(k) => self
                .plenums
                .iter()
                .filter(|p| p.key == k)
                .filter(visible)
                .collect(),
            None => {
                let here = self.plenums_in(room);
                if here.is_empty() {
                    self.plenums.iter().filter(visible).collect()
                } else {
                    here
                }
            }
        };
        if list.is_empty() {
            return format!("Kein Plenum „{}“.", key.unwrap_or(""));
        }
        let mut out = Vec::new();
        for p in list {
            let mut line = format!("📋 **{}** (`{}`)", p.name, p.key);
            match self.next_meeting(st, p, now) {
                Some((id, start)) => {
                    line.push_str(&format!(
                        "\nNächstes Mal: {}",
                        schedule::describe_meeting(start)
                    ));
                    if let Some(m) = st.plenums[&p.key].meetings.get(&id).filter(|m| m.created) {
                        line.push_str(&format!("\nProtokoll: {}", self.link_of(m)));
                    }
                }
                None => line.push_str("\nKein Termin im Kalender."),
            }
            match &self.calendar_url {
                Some(url) => line.push_str(&format!(
                    "\nTermine: Kalender „{}“ in Nextcloud ({url})",
                    p.name
                )),
                None => line.push_str(&format!("\nTermine: Kalender „{}“ in Nextcloud", p.name)),
            }
            if let Some(u) = st.plenums[&p.key].urls.get("overview") {
                line.push_str(&format!("\nÜbersicht: {}", self.wiki.link(u)));
            }
            let pending = st.plenums[&p.key].pending_tops.len();
            if pending > 0 {
                line.push_str(&format!(
                    "\n{pending} Punkt(e) warten auf das nächste Protokoll."
                ));
            }
            out.push(line);
        }
        out.join("\n\n")
    }

    async fn add_top(
        &self,
        st: &mut State,
        p: &Plenum,
        sender: &Sender,
        text: &str,
        now: DateTime<Utc>,
    ) -> String {
        let top = Top {
            text: text.to_owned(),
            by: sender.name.clone(),
            at: now,
        };
        let line = protocol::top_line(&top);
        // The open protocol of the next meeting, if it exists already.
        let target = self.next_meeting(st, p, now).and_then(|(id, _)| {
            st.plenums[&p.key]
                .meetings
                .get(&id)
                .filter(|m| m.created)
                .map(|m| (id.clone(), m.doc_id.clone(), self.link_of(m)))
        });
        let Some((key, doc_id, link)) = target else {
            st.plenums
                .get_mut(&p.key)
                .expect("next_meeting made it")
                .pending_tops
                .push(top);
            return match self.next_meeting(st, p, now) {
                Some((_, start)) => format!(
                    "📝 Notiert für das {} am {}. Der Punkt kommt ins Protokoll, sobald es angelegt ist.",
                    p.name,
                    schedule::describe_meeting(start)
                ),
                None => format!("📝 Notiert. Für das {} steht noch kein Termin im Kalender; der Punkt kommt ins nächste Protokoll.", p.name),
            };
        };
        match self.add_lines(&doc_id, std::slice::from_ref(&line)).await {
            Ok(Some(())) => {
                info!(plenum = %p.key, meeting = %key, "Agenda item added");
                format!("✅ Steht auf der Tagesordnung: {link}")
            }
            Ok(None) => "⚠️ Das Protokoll wurde gelöscht. Bitte die Admins fragen.".to_owned(),
            Err(e) => {
                warn!(plenum = %p.key, "Agenda item: {e:#}");
                "⚠️ Der Punkt ließ sich gerade nicht eintragen. Bitte direkt ins Protokoll schreiben oder es gleich nochmal versuchen.".to_owned()
            }
        }
    }

    fn request_close(&self, st: &mut State, p: &Plenum, now: DateTime<Utc>) -> String {
        let Some(key) = self.current_meeting(st, p, now) else {
            return format!(
                "Es gibt kein offenes Protokoll vom {}, das schon begonnen hat.",
                p.name
            );
        };
        let start = meeting(st, p, &key).start_of(&key);
        meeting(st, p, &key).close.requested = true;
        format!(
            "👍 Das Protokoll vom {} wird gleich abgeschlossen: Beschlüsse in die Liste, PDF, Zusammenfassung.",
            start.map(schedule::describe_meeting).unwrap_or_default()
        )
    }
}

impl<W: Wiki, C: Chat> Engine<W, C> {
    /// The rooms of plenums with a group follow it (see [`members`]). A room shared by
    /// plenums follows all their groups; a room with a plenum without a group (the house
    /// chat) is left alone, and so is one whose group the directory does not know or has
    /// nobody in.
    pub async fn sync_members(&self, groups: &Groups, now: DateTime<Utc>) {
        let Some(me) = self.me.clone() else {
            return;
        };
        let mut rooms: Vec<OwnedRoomId> = self.plenums.iter().map(|p| p.room.clone()).collect();
        rooms.sort();
        rooms.dedup();
        for room in rooms {
            let here: Vec<&Plenum> = self.plenums_in(&room);
            if here.iter().any(|p| p.group.is_none()) {
                continue;
            }
            let names: BTreeSet<&str> = here.iter().filter_map(|p| p.group.as_deref()).collect();
            if let Some(missing) = names.iter().find(|g| !groups.contains_key(**g)) {
                warn!(%room, group = %missing, "Members: the directory has nobody in the group; the room is left as it is");
                continue;
            }
            let wanted: BTreeSet<OwnedUserId> = names
                .iter()
                .flat_map(|g| groups[*g].iter().cloned())
                .collect();
            if let Err(e) = self.sync_room(&room, &wanted, &names, &me, now).await {
                warn!(%room, "Members: {e:#}");
            }
        }
    }

    async fn sync_room(
        &self,
        room: &RoomId,
        wanted: &BTreeSet<OwnedUserId>,
        groups: &BTreeSet<&str>,
        me: &UserId,
        now: DateTime<Utc>,
    ) -> Result<()> {
        let current = self.chat.members(room).await?;
        let mut st = self.state.lock().await;
        let remembered = st.rooms.entry(room.to_string()).or_default();
        members::forget_others(remembered, wanted);
        let plan = members::plan(wanted, &current, &self.keep, me, remembered, now);
        let groups: Vec<&str> = groups.iter().copied().collect();
        let reason = format!("nicht in der Gruppe {} (Hauskonto)", groups.join(", "));
        for user in &plan.remove {
            match self.chat.remove(room, user, &reason).await {
                Ok(()) => info!(%room, %user, "Members: removed (not in the group)"),
                Err(e) => warn!(%room, %user, "Members: removing failed: {e:#}"),
            }
        }
        for user in &plan.invite {
            let remembered = st.rooms.entry(room.to_string()).or_default();
            match self.chat.invite(room, user).await {
                Ok(()) => {
                    info!(%room, %user, "Members: invited (in the group)");
                    remembered.failed.remove(user.as_str());
                    remembered.invited.insert(user.to_string());
                }
                Err(e) => {
                    // an account that does not exist (yet): tried again a day later
                    info!(%room, %user, "Members: invitation failed, again in a day: {e:#}");
                    remembered.failed.insert(user.to_string(), now);
                }
            }
        }
        // who is in the room already counts as invited (leaving later is their choice)
        let remembered = st.rooms.entry(room.to_string()).or_default();
        for m in &current {
            if wanted.contains(&m.id) {
                remembered.invited.insert(m.id.to_string());
            }
        }
        self.save(&st).await;
        Ok(())
    }
}

/// Agenda lines in a protocol: list items between the agenda heading and the next heading.
fn count_tops(markdown: &str) -> usize {
    let mut inside = false;
    let mut n = 0;
    for line in markdown.lines() {
        let t = line.trim_start();
        if t.starts_with('#') {
            inside = t
                .trim_start_matches('#')
                .trim()
                .to_lowercase()
                .starts_with("tagesordnung");
            continue;
        }
        if inside
            && (t.starts_with("* ")
                || t.starts_with("- ")
                || t.chars().next().is_some_and(|c| c.is_ascii_digit()) && t.contains(". "))
        {
            n += 1;
        }
    }
    n
}

pub fn help() -> String {
    "📋 **Plenum-Bot**\n\n\
     `!plenum`: wann ist das nächste Plenum, wo ist das Protokoll\n\
     `!top <Thema>`: ein Punkt für die Tagesordnung\n\
     `!protokoll fertig`: das Protokoll jetzt abschließen (Beschlüsse, PDF, Zusammenfassung)\n\
     Die Termine stehen im Nextcloud-Kalender des Plenums: dort eintragen, verschieben, absagen \
     oder als Serie anlegen; der Bot folgt ihm.\n\n\
     Ein Thema geht auch so: auf eine Nachricht mit `!top` antworten.\n\
     Gibt es in einem Raum mehrere Plena, kommt der Name dazu: `!top kueche-eg Spülmaschine`.\n\
     Im Protokoll wird jede Zeile, die mit „Beschluss:“ beginnt, nach dem Plenum in die Beschlussliste übernommen \
     (außer in der Tagesordnung)."
        .to_owned()
}

/// Shared by the tests of this module.
#[cfg(test)]
pub(crate) mod fakes {
    use super::*;
    use anyhow::Context;
    use std::sync::Mutex as StdMutex;

    #[derive(Default)]
    pub struct FakeWiki {
        pub docs: StdMutex<Vec<(Doc, bool)>>, // (doc, deleted)
        pub collections: StdMutex<Vec<(String, String)>>,
        /// (collection, access) as last set
        pub access: StdMutex<Vec<(String, Access)>>,
        /// the wiki's groups, and (collection, group) grants
        pub groups: StdMutex<Vec<String>>,
        pub grants: StdMutex<Vec<(String, String)>>,
        pub attachments: StdMutex<Vec<(String, String, usize)>>,
        pub fail: StdMutex<bool>,
    }

    impl FakeWiki {
        pub fn by_title(&self, title: &str) -> Option<Doc> {
            self.docs
                .lock()
                .unwrap()
                .iter()
                .find(|(d, del)| d.title == title && !del)
                .map(|(d, _)| d.clone())
        }
        fn check(&self) -> Result<()> {
            if *self.fail.lock().unwrap() {
                anyhow::bail!("wiki down");
            }
            Ok(())
        }
    }

    impl Wiki for FakeWiki {
        async fn ensure_collection(&self, name: &str, access: Access) -> Result<String> {
            self.check()?;
            let mut c = self.collections.lock().unwrap();
            let id = match c.iter().find(|(_, n)| n == name) {
                Some((id, _)) => id.clone(),
                None => {
                    let id = format!("col-{}", c.len());
                    c.push((id.clone(), name.to_owned()));
                    id
                }
            };
            let mut a = self.access.lock().unwrap();
            a.retain(|(c, _)| c != &id);
            a.push((id.clone(), access));
            Ok(id)
        }
        async fn grant_group(&self, collection_id: &str, group: &str) -> Result<bool> {
            self.check()?;
            if !self.groups.lock().unwrap().iter().any(|g| g == group) {
                return Ok(false);
            }
            let mut g = self.grants.lock().unwrap();
            let grant = (collection_id.to_owned(), group.to_owned());
            if !g.contains(&grant) {
                g.push(grant);
            }
            Ok(true)
        }
        async fn move_doc(&self, id: &str, collection_id: &str, parent_id: &str) -> Result<()> {
            self.check()?;
            let mut docs = self.docs.lock().unwrap();
            let (d, _) = docs
                .iter_mut()
                .find(|(d, _)| d.id == id)
                .context("no doc")?;
            d.collection_id = Some(collection_id.to_owned());
            d.parent_id = Some(parent_id.to_owned());
            Ok(())
        }
        async fn rename(&self, id: &str, title: &str) -> Result<()> {
            self.check()?;
            let mut docs = self.docs.lock().unwrap();
            let (d, _) = docs
                .iter_mut()
                .find(|(d, _)| d.id == id)
                .context("no doc")?;
            d.title = title.to_owned();
            Ok(())
        }
        async fn replace(&self, id: &str, text: &str) -> Result<()> {
            self.check()?;
            let mut docs = self.docs.lock().unwrap();
            let (d, _) = docs
                .iter_mut()
                .find(|(d, _)| d.id == id)
                .context("no doc")?;
            d.text = text.to_owned();
            d.revision += 1;
            Ok(())
        }
        async fn find_doc(&self, collection_id: &str, title: &str) -> Result<Option<Doc>> {
            self.check()?;
            Ok(self
                .docs
                .lock()
                .unwrap()
                .iter()
                .find(|(d, del)| {
                    !del && d.title == title && d.collection_id.as_deref() == Some(collection_id)
                })
                .map(|(d, _)| d.clone()))
        }
        async fn create_doc(
            &self,
            id: &str,
            collection_id: &str,
            parent_id: Option<&str>,
            title: &str,
            text: &str,
        ) -> Result<Doc> {
            self.check()?;
            let mut docs = self.docs.lock().unwrap();
            if docs.iter().any(|(d, _)| d.id == id) {
                anyhow::bail!("id must be unique");
            }
            let d = Doc {
                id: id.to_owned(),
                title: title.to_owned(),
                text: text.to_owned(),
                url: format!("/doc/{}", title.to_lowercase().replace(' ', "-")),
                revision: 1,
                collection_id: Some(collection_id.to_owned()),
                parent_id: parent_id.map(str::to_owned),
            };
            docs.push((d.clone(), false));
            Ok(d)
        }
        async fn doc(&self, id: &str) -> Result<Option<Doc>> {
            self.check()?;
            Ok(self
                .docs
                .lock()
                .unwrap()
                .iter()
                .find(|(d, del)| d.id == id && !del)
                .map(|(d, _)| d.clone()))
        }
        async fn patch(&self, id: &str, find: &str, text: &str) -> Result<bool> {
            self.check()?;
            let mut docs = self.docs.lock().unwrap();
            let (d, _) = docs
                .iter_mut()
                .find(|(d, _)| d.id == id)
                .context("no doc")?;
            if !d.text.contains(find) {
                return Ok(false);
            }
            d.text = d.text.replacen(find, text, 1);
            d.revision += 1;
            Ok(true)
        }
        async fn append(&self, id: &str, text: &str) -> Result<()> {
            self.check()?;
            let mut docs = self.docs.lock().unwrap();
            let (d, _) = docs
                .iter_mut()
                .find(|(d, _)| d.id == id)
                .context("no doc")?;
            d.text.push_str(text);
            d.revision += 1;
            Ok(())
        }
        async fn attach(
            &self,
            doc_id: &str,
            name: &str,
            _ct: &str,
            bytes: Vec<u8>,
        ) -> Result<String> {
            self.check()?;
            let mut a = self.attachments.lock().unwrap();
            a.push((doc_id.to_owned(), name.to_owned(), bytes.len()));
            Ok(format!("/api/attachments.redirect?id=att-{}", a.len()))
        }
        fn link(&self, path: &str) -> String {
            format!("https://wiki.test{path}")
        }
    }

    #[derive(Default)]
    pub struct FakeChat {
        pub sent: StdMutex<Vec<(String, String, String)>>, // (room, text, txn)
        pub files: StdMutex<Vec<(String, String, usize)>>,
        pub fail: StdMutex<bool>,
        pub fail_files: StdMutex<bool>,
        /// (room, member) of every room
        pub members: StdMutex<Vec<(String, Member)>>,
        /// accounts an invitation fails for (they do not exist)
        pub unknown: StdMutex<Vec<String>>,
        pub removed: StdMutex<Vec<(String, String, String)>>, // (room, user, reason)
    }

    impl FakeChat {
        pub fn texts(&self) -> Vec<String> {
            self.sent
                .lock()
                .unwrap()
                .iter()
                .map(|(_, t, _)| t.clone())
                .collect()
        }
    }

    impl Chat for FakeChat {
        async fn send(&self, room: &RoomId, markdown: &str, txn: &str) -> Result<()> {
            if *self.fail.lock().unwrap() {
                anyhow::bail!("matrix down");
            }
            let mut sent = self.sent.lock().unwrap();
            // like a homeserver: a resend with the same transaction ID is dropped
            if !sent.iter().any(|(_, _, t)| t == txn) {
                sent.push((room.to_string(), markdown.to_owned(), txn.to_owned()));
            }
            Ok(())
        }
        async fn send_file(
            &self,
            room: &RoomId,
            name: &str,
            bytes: Vec<u8>,
            _txn: &str,
        ) -> Result<()> {
            if *self.fail.lock().unwrap() || *self.fail_files.lock().unwrap() {
                anyhow::bail!("matrix down");
            }
            self.files
                .lock()
                .unwrap()
                .push((room.to_string(), name.to_owned(), bytes.len()));
            Ok(())
        }
        async fn members(&self, room: &RoomId) -> Result<Vec<Member>> {
            Ok(self
                .members
                .lock()
                .unwrap()
                .iter()
                .filter(|(r, _)| r == room.as_str())
                .map(|(_, m)| m.clone())
                .collect())
        }
        async fn invite(&self, room: &RoomId, user: &UserId) -> Result<()> {
            if self
                .unknown
                .lock()
                .unwrap()
                .iter()
                .any(|u| u == user.as_str())
            {
                anyhow::bail!("M_NOT_FOUND");
            }
            self.members.lock().unwrap().push((
                room.to_string(),
                Member {
                    id: user.to_owned(),
                    admin: false,
                },
            ));
            Ok(())
        }
        async fn remove(&self, room: &RoomId, user: &UserId, reason: &str) -> Result<()> {
            self.members
                .lock()
                .unwrap()
                .retain(|(r, m)| !(r == room.as_str() && m.id == user));
            self.removed.lock().unwrap().push((
                room.to_string(),
                user.to_string(),
                reason.to_owned(),
            ));
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fakes::*;
    use super::*;
    use chrono::{Datelike, TimeZone};

    fn plenum(key: &str, name: &str, room: &str) -> Plenum {
        Plenum {
            key: key.into(),
            name: name.into(),
            room: room.try_into().unwrap(),
            collection: "Plena".into(),
            calendar: format!("plenum-{key}"),
            prepare_days: 3,
            remind_minutes: 30,
            duration_minutes: 120,
            close_after_hours: 12,
            organizers: HashSet::new(),
            group: None,
            access: Access::House,
        }
    }

    fn font_dir() -> Option<String> {
        ["/usr/share/fonts/truetype/dejavu", "/usr/share/fonts/TTF"]
            .into_iter()
            .find(|d| std::path::Path::new(d).join("DejaVuSans.ttf").exists())
            .map(str::to_owned)
    }

    struct T {
        engine: Engine<FakeWiki, FakeChat>,
        _dir: tempfile::TempDir,
    }

    fn setup(plenums: Vec<Plenum>) -> T {
        let dir = tempfile::tempdir().unwrap();
        let pdf = match font_dir() {
            Some(font_dir) => PdfConfig {
                enabled: true,
                font_dir,
                post_to_room: true,
            },
            None => PdfConfig {
                enabled: false,
                font_dir: String::new(),
                post_to_room: false,
            },
        };
        let engine = Engine::new(
            plenums,
            "Europe/Berlin".parse().unwrap(),
            FakeWiki::default(),
            FakeChat::default(),
            pdf,
            HashSet::from(["@admin:x".try_into().unwrap()]),
            State::default(),
            dir.path().join("state.json"),
        );
        // Every plenum's calendar: a series on the first Sunday of the month at 19:00.
        {
            let mut st = engine.state.try_lock().unwrap();
            for p in &engine.plenums {
                let ps = st.plenums.entry(p.key.clone()).or_default();
                ps.occurrences = first_sundays(&p.key);
                ps.calendar_read = Some(at(2026, 10, 1, 0, 0));
            }
        }
        T { engine, _dir: dir }
    }

    /// The first Sundays from October 2026 to June 2027, 19:00 to 21:00, as one series.
    fn first_sundays(key: &str) -> Vec<Occurrence> {
        (0..9)
            .map(|i| {
                let month = (9 + i) % 12 + 1;
                let year = if month >= 10 { 2026 } else { 2027 };
                let first = NaiveDate::from_ymd_opt(year, month, 1).unwrap();
                let day = first
                    + Duration::days(i64::from((7 - first.weekday().num_days_from_sunday()) % 7));
                occ(&format!("{key}-series#{day}"), &format!("{day} 19:00"))
            })
            .collect()
    }

    fn occ(id: &str, start: &str) -> Occurrence {
        let start: MeetingKey = start.parse().unwrap();
        Occurrence {
            id: id.into(),
            start,
            end: Some(start.0 + Duration::hours(2)),
            summary: "Plenum".into(),
        }
    }

    /// The calendar changed: `f` edits the plenum's occurrences.
    async fn calendar(
        e: &Engine<FakeWiki, FakeChat>,
        key: &str,
        f: impl FnOnce(&mut Vec<Occurrence>),
        now: DateTime<Utc>,
    ) {
        let mut list = e.state.lock().await.plenums[key].occurrences.clone();
        f(&mut list);
        e.apply_calendar(key, list, now).await;
    }

    /// Berlin local time (CET in November).
    fn at(y: i32, m: u32, d: u32, h: u32, min: u32) -> DateTime<Utc> {
        let tz: Tz = "Europe/Berlin".parse().unwrap();
        tz.with_ymd_and_hms(y, m, d, h, min, 0)
            .unwrap()
            .with_timezone(&Utc)
    }

    fn anna() -> Sender {
        Sender {
            id: "@anna:x".try_into().unwrap(),
            name: "Anna".into(),
        }
    }

    fn room() -> OwnedRoomId {
        "!haus:x".try_into().unwrap()
    }

    fn cmd(s: &str) -> Command {
        crate::commands::parse(s, NaiveDate::from_ymd_opt(2026, 10, 20).unwrap()).unwrap()
    }

    #[tokio::test]
    async fn a_meeting_from_preparation_to_summary() {
        let t = setup(vec![plenum("haus", "Hausplenum", "!haus:x")]);
        let e = &t.engine;

        // An item before the protocol exists waits for it.
        let a = e
            .command(&room(), &anna(), cmd("!top Müll"), at(2026, 10, 20, 12, 0))
            .await;
        assert!(a.contains("Notiert"), "{a}");

        e.tick(at(2026, 10, 28, 18, 59)).await; // 4 days before: nothing yet
        assert!(e.wiki.by_title("Hausplenum 2026-11-01").is_none());

        e.tick(at(2026, 10, 29, 19, 0)).await; // 3 days before: prepared and announced
        let doc = e.wiki.by_title("Hausplenum 2026-11-01").expect("protocol");
        assert!(doc.text.contains("* Müll (Anna)"), "{}", doc.text);
        assert!(e.wiki.by_title("Vorlage Hausplenum").is_some());
        assert!(e.wiki.by_title("Beschlüsse Hausplenum").is_some());
        let texts = e.chat.texts();
        assert_eq!(texts.len(), 1);
        assert!(
            texts[0].contains("https://wiki.test/doc/hausplenum-2026-11-01"),
            "{}",
            texts[0]
        );
        assert!(texts[0].contains("1 Punkt(e)"));

        // Ticks change nothing more; a second item goes straight into the document.
        e.tick(at(2026, 10, 29, 19, 1)).await;
        assert_eq!(e.chat.texts().len(), 1);
        let a = e
            .command(&room(), &anna(), cmd("!top Kaffee"), at(2026, 10, 30, 9, 0))
            .await;
        assert!(a.contains("Steht auf der Tagesordnung"), "{a}");
        let doc = e.wiki.by_title("Hausplenum 2026-11-01").unwrap();
        // In place of the marker, which stays below it for the next one.
        let marker = protocol::TOP_MARKER;
        assert!(
            doc.text
                .contains(&format!("* Müll (Anna)\n* Kaffee (Anna)\n\n{marker}"))
                || doc.text.contains(&format!("* Kaffee (Anna)\n\n{marker}")),
            "{}",
            doc.text
        );

        e.tick(at(2026, 11, 1, 18, 30)).await; // the reminder
        assert!(e.chat.texts()[1].contains("beginnt um 19:00"));

        // During the meeting: decisions are written.
        {
            let mut docs = e.wiki.docs.lock().unwrap();
            let (d, _) = docs
                .iter_mut()
                .find(|(d, _)| d.title == "Hausplenum 2026-11-01")
                .unwrap();
            d.text
                .push_str("\nBeschluss: Kompost kommt.\n- [ ] Ben: Tonne kaufen\n");
        }
        e.tick(at(2026, 11, 1, 21, 0)).await; // just ended: not closed yet
        assert_eq!(e.chat.texts().len(), 2);
        e.tick(at(2026, 11, 2, 9, 0)).await; // 12 h after the end: closed
        let texts = e.chat.texts();
        assert_eq!(texts.len(), 3, "{texts:?}");
        assert!(texts[2].contains("- Kompost kommt."), "{}", texts[2]);
        assert!(texts[2].contains("- Ben: Tonne kaufen"));
        let decisions = e.wiki.by_title("Beschlüsse Hausplenum").unwrap();
        assert!(decisions.text.contains("### [Hausplenum 2026-11-01](https://wiki.test/doc/hausplenum-2026-11-01)\n\n* Kompost kommt."), "{}", decisions.text);
        if e.pdf.enabled {
            assert_eq!(e.wiki.attachments.lock().unwrap().len(), 1);
            assert_eq!(e.chat.files.lock().unwrap().len(), 1);
            let doc = e.wiki.by_title("Hausplenum 2026-11-01").unwrap();
            assert!(doc.text.contains(
                "Das Protokoll als PDF:\n\n[Hausplenum 2026-11-01.pdf](/api/attachments.redirect?id=att-1)"
            ));
        }

        // Done is done.
        e.tick(at(2026, 11, 2, 10, 0)).await;
        assert_eq!(e.chat.texts().len(), 3);
        assert_eq!(
            decisions.text,
            e.wiki.by_title("Beschlüsse Hausplenum").unwrap().text
        );
    }

    #[tokio::test]
    async fn a_failed_step_is_repeated_and_the_done_ones_are_not() {
        let t = setup(vec![plenum("haus", "Hausplenum", "!haus:x")]);
        let e = &t.engine;
        *e.wiki.fail.lock().unwrap() = true;
        e.tick(at(2026, 10, 29, 19, 0)).await;
        assert!(e.wiki.docs.lock().unwrap().is_empty());
        *e.wiki.fail.lock().unwrap() = false;
        *e.chat.fail.lock().unwrap() = true; // created, but the announcement fails
        e.tick(at(2026, 10, 29, 19, 1)).await;
        assert!(e.wiki.by_title("Hausplenum 2026-11-01").is_some());
        assert!(e.chat.texts().is_empty());
        *e.chat.fail.lock().unwrap() = false;
        e.tick(at(2026, 10, 29, 19, 2)).await;
        e.tick(at(2026, 10, 29, 19, 3)).await;
        assert_eq!(e.chat.texts().len(), 1);
        let protocols = e
            .wiki
            .docs
            .lock()
            .unwrap()
            .iter()
            .filter(|(d, _)| d.title.starts_with("Hausplenum 2026"))
            .count();
        assert_eq!(protocols, 1, "no second protocol");
    }

    #[tokio::test]
    async fn a_restart_finds_the_protocol_it_created_before_the_crash() {
        let t = setup(vec![plenum("haus", "Hausplenum", "!haus:x")]);
        let e = &t.engine;
        e.tick(at(2026, 10, 29, 19, 0)).await;
        // As if the bot had crashed after creating the document but before saving it.
        {
            let mut st = e.state.lock().await;
            let m = st
                .plenums
                .get_mut("haus")
                .unwrap()
                .meetings
                .values_mut()
                .next()
                .unwrap();
            m.created = false;
            m.announce = Default::default();
        }
        e.tick(at(2026, 10, 29, 19, 1)).await;
        let protocols = e
            .wiki
            .docs
            .lock()
            .unwrap()
            .iter()
            .filter(|(d, _)| d.title.starts_with("Hausplenum 2026"))
            .count();
        assert_eq!(protocols, 1);
    }

    #[tokio::test]
    async fn the_bot_closes_early_on_request_and_skips_what_it_missed() {
        let t = setup(vec![plenum("haus", "Hausplenum", "!haus:x")]);
        let e = &t.engine;
        e.tick(at(2026, 10, 29, 19, 0)).await;
        let a = e
            .command(
                &room(),
                &anna(),
                cmd("!protokoll fertig"),
                at(2026, 10, 30, 12, 0),
            )
            .await;
        assert!(a.contains("kein offenes Protokoll"), "not started yet: {a}");
        let a = e
            .command(
                &room(),
                &anna(),
                cmd("!protokoll fertig"),
                at(2026, 11, 1, 20, 0),
            )
            .await;
        assert!(a.contains("wird gleich abgeschlossen"), "{a}");
        e.tick(at(2026, 11, 1, 20, 1)).await;
        assert!(e.chat.texts().last().unwrap().contains("ist abgeschlossen"));

        // The December meeting passes while the bot is down: skipped, not prepared late.
        e.tick(at(2026, 12, 7, 12, 0)).await;
        assert!(e.wiki.by_title("Hausplenum 2026-12-06").is_none());
        let st = e.state.lock().await;
        assert!(st.plenums["haus"].meetings["haus-series#2026-12-06"].skipped);
    }

    #[tokio::test]
    async fn the_calendar_cancels_adds_and_brings_back_dates() {
        let t = setup(vec![plenum("haus", "Hausplenum", "!haus:x")]);
        let e = &t.engine;
        let now = at(2026, 10, 20, 12, 0);
        // cancelled before it was prepared: no protocol, nothing said
        calendar(
            e,
            "haus",
            |l| l.retain(|o| o.id != "haus-series#2026-11-01"),
            now,
        )
        .await;
        // an extra date
        calendar(
            e,
            "haus",
            |l| l.push(occ("extra-1", "2026-11-04 20:00")),
            now,
        )
        .await;
        let a = e.command(&room(), &anna(), cmd("!plenum"), now).await;
        assert!(
            a.contains("Nächstes Mal: Mittwoch, 04.11.2026 um 20:00"),
            "{a}"
        );
        assert!(a.contains("Kalender „Hausplenum“"), "{a}");
        e.tick(at(2026, 10, 29, 19, 0)).await;
        assert!(e.wiki.by_title("Hausplenum 2026-11-01").is_none());
        e.tick(at(2026, 11, 1, 20, 0)).await;
        assert!(e.wiki.by_title("Hausplenum 2026-11-04").is_some());
        assert_eq!(e.chat.texts().len(), 1, "announced once");

        // announced, then cancelled: said once, the protocol stays
        e.tick(at(2026, 12, 3, 19, 0)).await; // 6.12. announced (and the 4.11. closed)
        let before = e.chat.texts().len();
        assert!(e.chat.texts().last().unwrap().contains("06.12.2026"));
        calendar(
            e,
            "haus",
            |l| l.retain(|o| o.id != "haus-series#2026-12-06"),
            at(2026, 12, 4, 9, 0),
        )
        .await;
        e.tick(at(2026, 12, 4, 9, 0)).await;
        e.tick(at(2026, 12, 4, 9, 1)).await;
        let texts = e.chat.texts();
        assert_eq!(texts.len(), before + 1, "{texts:?}");
        let last = texts.last().unwrap();
        assert!(
            last.contains("abgesagt") && last.contains("06.12.2026"),
            "{last}"
        );
        assert!(e.wiki.by_title("Hausplenum 2026-12-06").is_some());
        e.tick(at(2026, 12, 6, 18, 30)).await; // no reminder for it
        assert_eq!(e.chat.texts().len(), before + 1);

        // the old commands point to the calendar
        let a = e
            .command(&room(), &anna(), cmd("!plenum absagen 3.1."), now)
            .await;
        assert!(a.contains("Nextcloud-Kalender"), "{a}");
    }

    #[tokio::test]
    async fn a_cancelled_meeting_that_comes_back_goes_on() {
        let t = setup(vec![plenum("haus", "Hausplenum", "!haus:x")]);
        let e = &t.engine;
        e.tick(at(2026, 10, 29, 19, 0)).await; // 1.11. announced
        let gone = e.state.lock().await.plenums["haus"].occurrences.clone();
        calendar(
            e,
            "haus",
            |l| l.retain(|o| o.id != "haus-series#2026-11-01"),
            at(2026, 10, 30, 9, 0),
        )
        .await;
        e.tick(at(2026, 10, 30, 9, 0)).await;
        assert!(e.chat.texts().last().unwrap().contains("abgesagt"));
        // undone in the calendar: the meeting goes on with its protocol
        e.apply_calendar("haus", gone, at(2026, 10, 30, 10, 0))
            .await;
        e.tick(at(2026, 11, 1, 18, 30)).await;
        assert!(e.chat.texts().last().unwrap().contains("beginnt um 19:00"));
    }

    #[tokio::test]
    async fn the_calendar_unread_changes_nothing() {
        let t = setup(vec![plenum("haus", "Hausplenum", "!haus:x")]);
        let e = &t.engine;
        e.tick(at(2026, 10, 29, 19, 0)).await;
        // a calendar that was never read knows nothing: nothing is cancelled
        {
            let mut st = e.state.lock().await;
            let ps = st.plenums.get_mut("haus").unwrap();
            ps.calendar_read = None;
            ps.occurrences.clear();
        }
        e.tick(at(2026, 10, 30, 9, 0)).await;
        assert_eq!(e.chat.texts().len(), 1);
        assert!(!e.state.lock().await.plenums["haus"].meetings["haus-series#2026-11-01"].cancelled);
    }

    #[tokio::test]
    async fn several_plenums_in_one_room_need_the_key() {
        let t = setup(vec![
            plenum("kueche-eg", "Küchenplenum EG", "!kitchens:x"),
            plenum("kueche-og", "Küchenplenum OG", "!kitchens:x"),
        ]);
        let e = &t.engine;
        let r: OwnedRoomId = "!kitchens:x".try_into().unwrap();
        let now = at(2026, 10, 20, 12, 0);
        let a = e.command(&r, &anna(), cmd("!top Spülmaschine"), now).await;
        assert!(
            a.contains("mehrere Plena") && a.contains("`kueche-eg`"),
            "{a}"
        );
        let a = e
            .command(
                &r,
                &anna(),
                crate::commands::parse_for(
                    "!top kueche-og Spülmaschine",
                    now.date_naive(),
                    &["kueche-eg", "kueche-og"],
                )
                .unwrap(),
                now,
            )
            .await;
        assert!(a.contains("Notiert für das Küchenplenum OG"), "{a}");
        let a = e.command(&room(), &anna(), cmd("!top x"), now).await;
        assert!(a.contains("kein Plenum"), "{a}");
        // Both get their own template and decision list in the shared collection.
        e.tick(at(2026, 10, 29, 19, 0)).await;
        assert!(e.wiki.by_title("Vorlage Küchenplenum EG").is_some());
        assert!(e.wiki.by_title("Beschlüsse Küchenplenum OG").is_some());
        let og = e.wiki.by_title("Küchenplenum OG 2026-11-01").unwrap();
        assert!(og.text.contains("Spülmaschine"));
        let eg = e.wiki.by_title("Küchenplenum EG 2026-11-01").unwrap();
        assert!(!eg.text.contains("Spülmaschine"));
    }

    #[tokio::test]
    async fn a_template_edited_in_the_wiki_is_used() {
        let t = setup(vec![plenum("haus", "Hausplenum", "!haus:x")]);
        let e = &t.engine;
        e.tick(at(2026, 10, 29, 19, 0)).await; // creates the template (and November's protocol)
        {
            let mut docs = e.wiki.docs.lock().unwrap();
            let (d, _) = docs
                .iter_mut()
                .find(|(d, _)| d.title == "Vorlage Hausplenum")
                .unwrap();
            d.text = "Eigene Vorlage für {{plenum}} am {{datum}}\n\n## Tagesordnung\n".into();
        }
        e.tick(at(2026, 12, 3, 19, 0)).await;
        let dec = e.wiki.by_title("Hausplenum 2026-12-06").unwrap();
        assert!(
            dec.text
                .starts_with("Eigene Vorlage für Hausplenum am 06.12.2026"),
            "{}",
            dec.text
        );
    }

    fn edit_doc(e: &Engine<FakeWiki, FakeChat>, title: &str, f: impl FnOnce(&mut String)) {
        let mut docs = e.wiki.docs.lock().unwrap();
        let (d, _) = docs
            .iter_mut()
            .find(|(d, del)| d.title == title && !del)
            .unwrap();
        f(&mut d.text);
        d.revision += 1;
    }

    fn delete_doc(e: &Engine<FakeWiki, FakeChat>, title: &str) {
        let mut docs = e.wiki.docs.lock().unwrap();
        docs.iter_mut().find(|(d, _)| d.title == title).unwrap().1 = true;
    }

    #[tokio::test]
    async fn a_chat_item_changes_nothing_else_and_decides_nothing() {
        let t = setup(vec![plenum("haus", "Hausplenum", "!haus:x")]);
        let e = &t.engine;
        e.tick(at(2026, 10, 29, 19, 0)).await;
        edit_doc(e, "Hausplenum 2026-11-01", |t| {
            t.push_str("\n* Variante [x] Hof oder [ ] Keller\n")
        });
        let a = e
            .command(
                &room(),
                &anna(),
                cmd("!top Beschluss: Kasse an Anna [ ] sofort"),
                at(2026, 10, 30, 9, 0),
            )
            .await;
        assert!(a.contains("Steht auf der Tagesordnung"), "{a}");
        let doc = e.wiki.by_title("Hausplenum 2026-11-01").unwrap();
        assert!(
            doc.text.contains("* Variante [x] Hof oder [ ] Keller"),
            "untouched: {}",
            doc.text
        );
        assert!(
            doc.text
                .contains("* TOP Beschluss: Kasse an Anna ( ) sofort (Anna)"),
            "{}",
            doc.text
        );
        let a = e
            .command(
                &room(),
                &anna(),
                cmd("!protokoll fertig"),
                at(2026, 11, 1, 20, 0),
            )
            .await;
        assert!(a.contains("wird gleich"), "{a}");
        e.tick(at(2026, 11, 1, 20, 1)).await;
        let summary = e.chat.texts().last().unwrap().clone();
        assert!(summary.contains("Keine Beschlüsse"), "{summary}");
        assert!(!summary.contains("Kasse"), "{summary}");
    }

    #[tokio::test]
    async fn without_the_marker_items_go_under_a_heading_of_their_own() {
        let t = setup(vec![plenum("haus", "Hausplenum", "!haus:x")]);
        let e = &t.engine;
        e.tick(at(2026, 10, 29, 19, 0)).await;
        edit_doc(e, "Hausplenum 2026-11-01", |t| {
            *t = t.replace(protocol::TOP_MARKER, "")
        });
        for item in ["!top Garten", "!top Keller"] {
            e.command(&room(), &anna(), cmd(item), at(2026, 10, 30, 9, 0))
                .await;
        }
        let doc = e.wiki.by_title("Hausplenum 2026-11-01").unwrap();
        assert!(
            doc.text
                .ends_with("## Neue Punkte aus dem Chat\n\n* Garten (Anna)\n\n* Keller (Anna)"),
            "{}",
            doc.text
        );
        // Sent again (a lost answer, a second try): not twice.
        e.command(&room(), &anna(), cmd("!top Garten"), at(2026, 10, 30, 9, 1))
            .await;
        let doc = e.wiki.by_title("Hausplenum 2026-11-01").unwrap();
        assert_eq!(doc.text.matches("Garten").count(), 1);
    }

    #[tokio::test]
    async fn a_deleted_decision_list_or_collection_comes_back() {
        let t = setup(vec![plenum("haus", "Hausplenum", "!haus:x")]);
        let e = &t.engine;
        e.tick(at(2026, 10, 29, 19, 0)).await;
        delete_doc(e, "Beschlüsse Hausplenum");
        edit_doc(e, "Hausplenum 2026-11-01", |t| {
            t.push_str("\nBeschluss: Kompost.\n")
        });
        e.tick(at(2026, 11, 2, 9, 0)).await;
        let list = e
            .wiki
            .by_title("Beschlüsse Hausplenum")
            .expect("created again");
        assert!(list.text.contains("Kompost."), "{}", list.text);
        assert!(e.chat.texts().last().unwrap().contains("ist abgeschlossen"));
        // The collection is looked up again for the next protocol.
        e.wiki.collections.lock().unwrap().clear();
        e.tick(at(2026, 12, 3, 19, 0)).await;
        assert!(e.wiki.by_title("Hausplenum 2026-12-06").is_some());
    }

    #[tokio::test]
    async fn one_failing_meeting_does_not_hold_up_the_next() {
        let mut p = plenum("haus", "Hausplenum", "!haus:x");
        p.prepare_days = 8;
        let t = setup(vec![p]);
        let e = &t.engine;
        calendar(
            e,
            "haus",
            |l| l.push(occ("weekly#2026-11-08", "2026-11-08 19:00")),
            at(2026, 10, 29, 0, 0),
        )
        .await;
        e.tick(at(2026, 10, 29, 19, 0)).await; // Nov 1 and Nov 8 prepared
        delete_doc(e, "Hausplenum 2026-11-01");
        // Nov 1 closes without a protocol; Nov 8 is unaffected.
        e.tick(at(2026, 11, 2, 9, 0)).await;
        assert!(e.wiki.by_title("Hausplenum 2026-11-08").is_some());
        let st = e.state.lock().await;
        assert!(
            st.plenums["haus"].meetings["haus-series#2026-11-01"]
                .close
                .done
        );
    }

    #[tokio::test]
    async fn a_pdf_that_does_not_reach_the_room_is_still_in_the_wiki() {
        let t = setup(vec![plenum("haus", "Hausplenum", "!haus:x")]);
        let e = &t.engine;
        if !e.pdf.enabled {
            return;
        }
        e.tick(at(2026, 10, 29, 19, 0)).await;
        *e.chat.fail_files.lock().unwrap() = true;
        for minute in 0..PDF_ATTEMPTS {
            e.tick(at(2026, 11, 2, 9, minute)).await;
        }
        let summary = e.chat.texts().last().unwrap().clone();
        assert!(summary.contains("ist abgeschlossen"), "{summary}");
        assert!(summary.contains("hängt im Wiki"), "{summary}");
        assert!(!summary.contains("nicht erstellen"), "{summary}");
        assert_eq!(e.wiki.attachments.lock().unwrap().len(), 1, "attached once");
    }

    #[tokio::test]
    async fn two_meetings_on_one_day_get_two_protocols() {
        let t = setup(vec![plenum("haus", "Hausplenum", "!haus:x")]);
        let e = &t.engine;
        calendar(
            e,
            "haus",
            |l| l.push(occ("noon", "2026-11-01 12:00")),
            at(2026, 10, 28, 0, 0),
        )
        .await;
        e.tick(at(2026, 10, 29, 19, 0)).await;
        let titles: Vec<String> = e
            .wiki
            .docs
            .lock()
            .unwrap()
            .iter()
            .map(|(d, _)| d.title.clone())
            .filter(|t| t.starts_with("Hausplenum 2026-11-01"))
            .collect();
        assert_eq!(titles.len(), 2, "{titles:?}");
        assert!(
            titles
                .iter()
                .any(|t| t.ends_with(" 19:00") || t.ends_with(" 12:00")),
            "{titles:?}"
        );
    }

    #[tokio::test]
    async fn organizers_close_early_and_others_may_not() {
        let mut p = plenum("haus", "Hausplenum", "!haus:x");
        p.organizers = HashSet::from(["@ben:x".try_into().unwrap()]);
        let t = setup(vec![p]);
        let e = &t.engine;
        e.tick(at(2026, 10, 29, 19, 0)).await;
        let a = e
            .command(
                &room(),
                &anna(),
                cmd("!protokoll fertig"),
                at(2026, 11, 1, 20, 0),
            )
            .await;
        assert!(a.starts_with("⛔"), "{a}");
        let ben = Sender {
            id: "@ben:x".try_into().unwrap(),
            name: "Ben".into(),
        };
        let a = e
            .command(
                &room(),
                &ben,
                cmd("!protokoll fertig"),
                at(2026, 11, 1, 20, 0),
            )
            .await;
        assert!(a.contains("wird gleich abgeschlossen"), "{a}");
    }

    #[tokio::test]
    async fn a_reminder_after_the_start_says_so() {
        let t = setup(vec![plenum("haus", "Hausplenum", "!haus:x")]);
        let e = &t.engine;
        e.tick(at(2026, 11, 1, 19, 30)).await; // the bot comes back during the meeting
        let texts = e.chat.texts();
        assert!(
            texts.last().unwrap().contains("hat um 19:00 Uhr begonnen"),
            "{texts:?}"
        );
    }

    #[test]
    fn agenda_items_are_counted_in_their_section() {
        let md = "## Tagesordnung\n\n* a\n* b\n1. c\n\n## Protokoll\n\n* not\n";
        assert_eq!(count_tops(md), 3);
    }

    fn kitchen() -> Plenum {
        Plenum {
            key: "kitchen-floor-1".into(),
            name: "Küchenplenum 1. OG".into(),
            room: "!k1:x".try_into().unwrap(),
            collection: "Küche 1. OG".into(),
            group: Some("kitchen-floor-1".into()),
            access: Access::Members,
            ..plenum("kitchen-floor-1", "Küchenplenum 1. OG", "!k1:x")
        }
    }

    fn finance() -> Plenum {
        Plenum {
            collection: "Finanz-AG".into(),
            group: Some("finance".into()),
            access: Access::Private,
            ..plenum("finance", "Finanzplenum", "!fin:x")
        }
    }

    #[tokio::test]
    async fn every_plenum_gets_its_area_with_the_access_its_group_needs() {
        let t = setup(vec![
            plenum("haus", "Hausplenum", "!haus:x"),
            kitchen(),
            finance(),
        ]);
        let e = &t.engine;
        e.wiki.groups.lock().unwrap().push("kitchen-floor-1".into()); // finance: not yet in the wiki
        e.tick(at(2026, 10, 20, 12, 0)).await;
        for title in [
            "Übersicht Küchenplenum 1. OG",
            "Regeln Küchenplenum 1. OG",
            "Protokolle Küchenplenum 1. OG",
            "Beschlüsse Küchenplenum 1. OG",
            "Vorlage Küchenplenum 1. OG",
            "Übersicht Hausplenum",
        ] {
            assert!(e.wiki.by_title(title).is_some(), "{title}");
        }
        let access = e.wiki.access.lock().unwrap().clone();
        let cols = e.wiki.collections.lock().unwrap().clone();
        let of = |name: &str| {
            let id = &cols.iter().find(|(_, n)| n == name).unwrap().0;
            access.iter().find(|(c, _)| c == id).unwrap().1
        };
        assert_eq!(of("Plena"), Access::House);
        assert_eq!(of("Küche 1. OG"), Access::Members);
        assert_eq!(of("Finanz-AG"), Access::Private);
        let grants = e.wiki.grants.lock().unwrap().clone();
        assert_eq!(grants.len(), 1, "{grants:?}");
        assert_eq!(grants[0].1, "kitchen-floor-1");
        // once the wiki has the finance group, it gets it at the next check (an hour later)
        e.wiki.groups.lock().unwrap().push("finance".into());
        e.tick(at(2026, 10, 20, 12, 30)).await;
        assert_eq!(e.wiki.grants.lock().unwrap().len(), 1);
        e.tick(at(2026, 10, 20, 13, 1)).await;
        assert_eq!(e.wiki.grants.lock().unwrap().len(), 2);
        // the overview: the next dates, how to bring in a topic, the links
        let o = e.wiki.by_title("Übersicht Hausplenum").unwrap();
        assert!(
            o.text.contains("So, 01.11.2026") || o.text.contains("01.11.2026"),
            "{}",
            o.text
        );
        assert!(
            o.text
                .contains("[Regeln Hausplenum](https://wiki.test/doc/regeln-hausplenum)"),
            "{}",
            o.text
        );
        // written only when it changes
        let rev = o.revision;
        e.tick(at(2026, 10, 20, 13, 2)).await;
        assert_eq!(
            e.wiki.by_title("Übersicht Hausplenum").unwrap().revision,
            rev
        );
    }

    #[tokio::test]
    async fn protocols_are_filed_and_the_old_ones_moved_there_once() {
        let t = setup(vec![plenum("haus", "Hausplenum", "!haus:x")]);
        let e = &t.engine;
        // a protocol of an older version, at the top of the collection
        {
            let mut st = e.state.lock().await;
            let ps = st.plenums.entry("haus".into()).or_default();
            ps.meetings.insert(
                "haus-series#2026-10-04".into(),
                Meeting {
                    doc_id: "old".into(),
                    created: true,
                    ..Default::default()
                },
            );
            ps.meetings
                .get_mut("haus-series#2026-10-04")
                .unwrap()
                .close
                .done = true;
        }
        let col = e
            .wiki
            .ensure_collection("Plena", Access::House)
            .await
            .unwrap();
        e.wiki
            .create_doc("old", &col, None, "Hausplenum 2026-10-04", "alt")
            .await
            .unwrap();
        e.tick(at(2026, 10, 29, 19, 0)).await; // prepares the 1.11. too
        let protocols = e.wiki.by_title("Protokolle Hausplenum").unwrap();
        let old = e.wiki.by_title("Hausplenum 2026-10-04").unwrap();
        let new = e.wiki.by_title("Hausplenum 2026-11-01").unwrap();
        assert_eq!(old.parent_id.as_deref(), Some(protocols.id.as_str()));
        assert_eq!(new.parent_id.as_deref(), Some(protocols.id.as_str()));
        // someone files it elsewhere: left there
        e.wiki.move_doc("old", &col, "elsewhere").await.unwrap();
        e.tick(at(2026, 10, 29, 21, 0)).await;
        assert_eq!(
            e.wiki
                .by_title("Hausplenum 2026-10-04")
                .unwrap()
                .parent_id
                .as_deref(),
            Some("elsewhere")
        );
    }

    #[tokio::test]
    async fn open_tasks_come_along_to_the_next_protocol_and_the_overview() {
        let t = setup(vec![plenum("haus", "Hausplenum", "!haus:x")]);
        let e = &t.engine;
        e.tick(at(2026, 10, 29, 19, 0)).await;
        {
            let mut docs = e.wiki.docs.lock().unwrap();
            let (d, _) = docs
                .iter_mut()
                .find(|(d, _)| d.title == "Hausplenum 2026-11-01")
                .unwrap();
            d.text.push_str(
                "\nBeschluss: Kompost kommt.\n- [ ] Ben: Tonne kaufen\n- [x] Cara: Liste\n",
            );
        }
        e.tick(at(2026, 11, 2, 9, 0)).await; // closed
        let o = e.wiki.by_title("Übersicht Hausplenum").unwrap();
        assert!(
            o.text
                .contains("* Ben: Tonne kaufen (seit dem [Plenum am 01.11.2026]"),
            "{}",
            o.text
        );
        assert!(
            o.text.contains("* Kompost kommt. ([01.11.2026]"),
            "{}",
            o.text
        );
        assert!(!o.text.contains("Cara: Liste"));
        // the next meeting (3rd Sunday? the rule here: 1st Sunday): 6.12., prepared 3.12.
        e.tick(at(2026, 12, 3, 19, 0)).await;
        let next = e
            .wiki
            .by_title("Hausplenum 2026-12-06")
            .expect("next protocol");
        assert!(
            next.text
                .contains("- [ ] Ben: Tonne kaufen (seit 01.11.2026)"),
            "{}",
            next.text
        );
        assert!(!next.text.contains("Cara"));
    }

    #[tokio::test]
    async fn a_meeting_moved_in_the_calendar_keeps_its_protocol() {
        let t = setup(vec![plenum("haus", "Hausplenum", "!haus:x")]);
        let e = &t.engine;
        e.tick(at(2026, 10, 29, 19, 0)).await; // 1.11. prepared and announced
        assert_eq!(e.chat.texts().len(), 1);
        let doc = e.wiki.by_title("Hausplenum 2026-11-01").unwrap();
        // the same occurrence a week later at 18:00 (RECURRENCE-ID: the ID stays)
        let move_to = |l: &mut Vec<Occurrence>| {
            for o in l.iter_mut().filter(|o| o.id == "haus-series#2026-11-01") {
                *o = occ(&o.id, "2026-11-08 18:00");
            }
        };
        calendar(e, "haus", move_to, at(2026, 10, 30, 9, 0)).await;
        e.tick(at(2026, 10, 30, 9, 0)).await;
        assert!(e.wiki.by_title("Hausplenum 2026-11-01").is_none());
        let moved = e.wiki.by_title("Hausplenum 2026-11-08").expect("renamed");
        assert_eq!(moved.id, doc.id, "the same protocol");
        // nothing at the old time; announced again prepare_days before, saying from when
        e.tick(at(2026, 11, 1, 18, 30)).await;
        assert_eq!(e.chat.texts().len(), 1);
        e.tick(at(2026, 11, 5, 18, 0)).await;
        let texts = e.chat.texts();
        assert_eq!(texts.len(), 2, "{texts:?}");
        assert!(
            texts[1].contains("08.11.2026 um 18:00")
                && texts[1].contains("verschoben vom Sonntag, 01.11.2026"),
            "{}",
            texts[1]
        );
        e.tick(at(2026, 11, 8, 17, 30)).await;
        assert!(e.chat.texts()[2].contains("beginnt um 18:00"));
        let a = e
            .command(&room(), &anna(), cmd("!plenum"), at(2026, 11, 8, 17, 31))
            .await;
        assert!(
            a.contains("08.11.2026 um 18:00") && a.contains(&moved.url),
            "{a}"
        );
    }

    #[tokio::test]
    async fn a_meeting_from_before_the_calendar_is_taken_over() {
        let t = setup(vec![plenum("haus", "Hausplenum", "!haus:x")]);
        let e = &t.engine;
        // the old state: keyed by its start, prepared and announced already
        {
            let mut st = e.state.lock().await;
            let ps = st.plenums.get_mut("haus").unwrap();
            let mut m = Meeting {
                doc_id: "old-doc".into(),
                created: true,
                url: Some("/doc/old".into()),
                ..Default::default()
            };
            m.announce.done = true;
            ps.meetings.insert("2026-11-01 19:00".into(), m);
        }
        let col = e
            .wiki
            .ensure_collection("Plena", Access::House)
            .await
            .unwrap();
        e.wiki
            .create_doc("old-doc", &col, None, "Hausplenum 2026-11-01", "alt")
            .await
            .unwrap();
        e.tick(at(2026, 10, 30, 9, 0)).await;
        let st = e.state.lock().await;
        let ps = &st.plenums["haus"];
        assert!(!ps.meetings.contains_key("2026-11-01 19:00"));
        let m = &ps.meetings["haus-series#2026-11-01"];
        assert_eq!(m.doc_id, "old-doc");
        assert!(m.announce.done && !m.cancelled);
        drop(st);
        assert!(
            e.chat.texts().is_empty(),
            "not announced twice, not cancelled"
        );
    }

    #[tokio::test]
    async fn a_groups_room_follows_the_group() {
        let t = setup(vec![kitchen()]);
        let mut e = t.engine;
        e.me = Some("@plenum-bot:x".try_into().unwrap());
        e.keep = HashSet::from(["@calendar-bot:x".try_into().unwrap()]);
        let m = |id: &str, admin: bool| {
            (
                "!k1:x".to_owned(),
                Member {
                    id: id.try_into().unwrap(),
                    admin,
                },
            )
        };
        e.chat.members.lock().unwrap().extend([
            m("@plenum-bot:x", true),
            m("@calendar-bot:x", false),
            m("@nick:x", true),
            m("@anna:x", false),
            m("@eve:x", false),
        ]);
        e.chat.unknown.lock().unwrap().push("@ghost:x".into());
        let groups: Groups = [(
            "kitchen-floor-1".to_owned(),
            ["@anna:x", "@ben:x", "@ghost:x"]
                .iter()
                .map(|s| (*s).try_into().unwrap())
                .collect(),
        )]
        .into();
        e.sync_members(&groups, at(2026, 10, 22, 10, 0)).await;
        let here: Vec<String> = e
            .chat
            .members
            .lock()
            .unwrap()
            .iter()
            .map(|(_, m)| m.id.to_string())
            .collect();
        assert!(here.contains(&"@ben:x".to_owned()));
        assert!(!here.contains(&"@eve:x".to_owned()));
        for kept in ["@plenum-bot:x", "@calendar-bot:x", "@nick:x", "@anna:x"] {
            assert!(here.contains(&kept.to_owned()), "{kept}");
        }
        let removed = e.chat.removed.lock().unwrap().clone();
        assert_eq!(removed.len(), 1);
        assert!(removed[0].2.contains("kitchen-floor-1"));
        // ben leaves on his own: not invited again; the ghost is tried again a day later
        e.chat
            .members
            .lock()
            .unwrap()
            .retain(|(_, m)| m.id != "@ben:x");
        e.sync_members(&groups, at(2026, 10, 22, 10, 15)).await;
        assert!(!e
            .chat
            .members
            .lock()
            .unwrap()
            .iter()
            .any(|(_, m)| m.id == "@ben:x"));
        e.chat.unknown.lock().unwrap().clear();
        e.sync_members(&groups, at(2026, 10, 22, 12, 0)).await;
        assert!(!e
            .chat
            .members
            .lock()
            .unwrap()
            .iter()
            .any(|(_, m)| m.id == "@ghost:x"));
        e.sync_members(&groups, at(2026, 10, 23, 11, 0)).await;
        assert!(e
            .chat
            .members
            .lock()
            .unwrap()
            .iter()
            .any(|(_, m)| m.id == "@ghost:x"));
        // a group the directory has nobody in: the room is left alone
        let before = e.chat.members.lock().unwrap().len();
        e.sync_members(&Groups::new(), at(2026, 10, 23, 12, 0))
            .await;
        assert_eq!(e.chat.members.lock().unwrap().len(), before);
    }

    #[tokio::test]
    async fn a_room_with_a_plenum_without_group_is_never_managed() {
        let t = setup(vec![plenum("haus", "Hausplenum", "!haus:x")]);
        let mut e = t.engine;
        e.me = Some("@plenum-bot:x".try_into().unwrap());
        e.chat.members.lock().unwrap().push((
            "!haus:x".into(),
            Member {
                id: "@eve:x".try_into().unwrap(),
                admin: false,
            },
        ));
        e.sync_members(&Groups::new(), at(2026, 10, 22, 10, 0))
            .await;
        assert!(e.chat.removed.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_private_plenum_shows_only_in_its_room_and_to_admins() {
        let t = setup(vec![plenum("haus", "Hausplenum", "!haus:x"), finance()]);
        let e = &t.engine;
        let elsewhere: OwnedRoomId = "!botroom:x".try_into().unwrap();
        let now = at(2026, 10, 20, 12, 0);
        let a = e.command(&elsewhere, &anna(), cmd("!plenum"), now).await;
        assert!(
            a.contains("Hausplenum") && !a.contains("Finanzplenum"),
            "{a}"
        );
        let fin: OwnedRoomId = "!fin:x".try_into().unwrap();
        let a = e.command(&fin, &anna(), cmd("!plenum"), now).await;
        assert!(a.contains("Finanzplenum"), "{a}");
        let a = e
            .command_in(&elsewhere, &anna(), cmd("!plenum"), now, Scope::AdminDm)
            .await;
        assert!(
            a.contains("Finanzplenum") && a.contains("Hausplenum"),
            "{a}"
        );
    }
}
