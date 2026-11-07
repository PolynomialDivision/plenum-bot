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
//!   close    (`close_after_hours` after the end, or `!minutes done`: decisions into the
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
//! it (`sync_members`, mxbot-common `membership`): one account per person and room, the one
//! they chose, with a handover for the others that never removes a person's last account in
//! the room (the chosen one is invited once more per handover when it is not there, and the
//! others stay while it is neither joined nor invited).
//!
//! The bot never rewrites a protocol people write in: chat items go in with Outline's
//! `patch` (only the marker line is replaced), everything else is appended. A full rewrite
//! would go through Outline's Markdown, which does not survive every text (a literal `[x]`
//! comes back as a checkbox).

use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    path::PathBuf,
    sync::Arc,
};

use anyhow::{Context, Result};
use chrono::{DateTime, Duration, NaiveDate, Utc};
use chrono_tz::Tz;
use mxbot_common::{
    admin::Admins,
    directory::{Choice, Contact, Invalid, People, Snapshot},
    matrix_sdk::ruma::{OwnedRoomId, OwnedUserId, RoomId, UserId},
    membership::{self, Confirm, Gate, Member, Presence, Rule},
};
use tokio::sync::Mutex;
use tracing::{error, info, warn};

use crate::{
    calendar::Occurrence,
    commands::{Command, When},
    config::{self, Access, Messages, PdfConfig, Plenum, Publication},
    former, page, plenum_settings,
    protocol::{self, PdfNote},
    schedule::{self, MeetingKey},
    state::{Carried, CarriedTopic, LinkMessage, Meeting, State, Top},
    stay,
};

/// A document as the bot needs it.
#[derive(Clone, Debug, PartialEq)]
pub struct Doc {
    pub id: String,
    pub title: String,
    pub text: String,
    pub url: String,
    /// Nobody (the bot neither) changed it since it was made.
    pub untouched: bool,
    pub collection_id: Option<String>,
    pub parent_id: Option<String>,
}

/// The wiki (Outline in production).
#[allow(async_fn_in_trait)]
pub trait Wiki {
    /// The collection's ID: `known` (the one used last time, renamed to `name` if its name
    /// differs) while it exists, else the one named `name` (never for a private one: someone
    /// else's would keep its members), else a new one; set to `access` (everyone writes;
    /// everyone reads; nobody but its groups) if it is not. `group`: the plenum's own group
    /// ([`Wiki::grant_group`]), so a house group that is it too keeps writing (`outline.rs`).
    async fn ensure_collection(
        &self,
        known: Option<&str>,
        name: &str,
        access: Access,
        group: Option<&str>,
    ) -> Result<String>;
    /// The wiki group `group` may write in the collection; false when the wiki has no such
    /// group (yet: outline-roles makes it from the directory).
    async fn grant_group(&self, collection_id: &str, group: &str) -> Result<bool>;
    /// The collection's pins `(pin, document)`, shown above its documents; None when the
    /// wiki does not let the bot see or make them (its key has no `pins.*`).
    async fn pins(&self, collection_id: &str) -> Result<Option<Vec<(String, String)>>>;
    /// The document pinned in the collection (once); false when the wiki does not let the bot.
    async fn pin(&self, collection_id: &str, doc_id: &str) -> Result<bool>;
    /// false when the wiki does not let the bot; a pin that is gone counts as removed.
    async fn unpin(&self, pin_id: &str) -> Result<bool>;
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
    /// Send Markdown with this transaction ID (a resend with the same ID is dropped). Every
    /// message into a plenum's room says what it is ([`Notice`]): [`sends`] decides.
    async fn send(&self, room: &RoomId, markdown: &str, notice: Notice, txn: &str) -> Result<()>;
    /// [`Self::send`], pinging `users` (Matrix mentions).
    async fn send_mentioning(
        &self,
        room: &RoomId,
        markdown: &str,
        users: &[OwnedUserId],
        notice: Notice,
        txn: &str,
    ) -> Result<()>;
    async fn send_file(
        &self,
        room: &RoomId,
        name: &str,
        bytes: Vec<u8>,
        notice: Notice,
        txn: &str,
    ) -> Result<()>;
    /// [`Self::send`], giving back the message's event ID (to edit it later).
    async fn post(
        &self,
        room: &RoomId,
        markdown: &str,
        notice: Notice,
        txn: &str,
    ) -> Result<String>;
    /// Replace the text of the bot's own message `event` (`txn` as for [`Self::send`]).
    async fn edit(&self, room: &RoomId, event: &str, markdown: &str, txn: &str) -> Result<()>;
    /// Who is in the room, joined or invited.
    async fn members(&self, room: &RoomId) -> Result<Vec<Member>>;
    async fn invite(&self, room: &RoomId, user: &UserId) -> Result<()>;
    /// Remove someone (also an open invitation), with the reason they see.
    async fn remove(&self, room: &RoomId, user: &UserId, reason: &str) -> Result<()>;
    /// Tell the admins, in their direct chats with the bot. `key` is the same for the same
    /// notice: a resend (after a restart) is dropped.
    async fn tell_admins(&self, markdown: &str, key: &str) -> Result<()>;
    /// Write to `user` in a direct chat with the bot (made when there is none); `key` as for
    /// [`Self::tell_admins`].
    async fn message_person(&self, user: &UserId, markdown: &str, key: &str) -> Result<()>;
    /// The room an alias (`#name:server`) points to.
    async fn resolve_alias(&self, alias: &str) -> Result<OwnedRoomId>;
}

/// Where a command comes from: what the overview may show there.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Scope {
    /// A plenum's room, or the bot's own room: private plenums only in their own room.
    Room,
    /// An admin's private chat with the bot: everything.
    AdminDm,
}

/// What a message into a plenum's room is (docs/PLENUM-PAGES.md §5).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Notice {
    /// The link to a meeting's new page (the one message of `messages quiet`).
    Link,
    /// The meeting moved or is back after a cancellation: a new message with the link.
    Moved,
    Reminder,
    Cancelled,
    /// The summary or the closed note after the meeting.
    Summary,
    Pdf,
    /// A setting changed from somewhere else (an admin's direct chat).
    Setting,
}

/// Whether the bot posts `notice` by itself in a plenum with these `messages`.
pub fn sends(messages: Messages, notice: Notice) -> bool {
    messages == Messages::All || notice == Notice::Link
}

/// What a cancelled meeting's page has after its title.
const CANCELLED: &str = " (cancelled)";

/// Attempts at the result box before the close goes on without it.
const BOX_ATTEMPTS: u32 = 3;

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
    /// The configured admins and those of `[security] admin_groups` (the members loop keeps
    /// them in line with the directory).
    pub admins: Admins,
    /// The bot's own account (never removed from a room); None: rooms are not managed.
    pub me: Option<OwnedUserId>,
    /// `[members] keep`: never removed from a room.
    pub keep: HashSet<OwnedUserId>,
    /// `[members] ask_to_choose`: write to members who have not chosen their account.
    pub ask_to_choose: bool,
    /// `[members] choose_url`: where they choose it.
    pub choose_url: Option<String>,
    /// `[members] handover_days`, `handover_max_days`: how long a member's other accounts stay
    /// in a room after they chose one.
    pub grace: membership::Grace,
    /// How old the directory's last good read may be before no room changes
    /// (`[directory] stale_after_minutes`).
    pub stale_after: std::time::Duration,
    /// `[members] subtenant_groups`: they live here too, for the kitchens' rooms ([`room_rule`]).
    pub subtenant_groups: Vec<String>,
    /// `[members] stay_reminder_days`: the admins hear of a stay's end this many days before
    /// ([`crate::stay`]).
    pub stay_reminder_days: u32,
    /// The directory's last good read (for an admin's `!members confirm`).
    snapshot: std::sync::Mutex<Option<Arc<Snapshot>>>,
    /// One round of the rooms at a time: the directory loop's and an admin's confirmation.
    members_round: Mutex<()>,
    /// Where people open the calendars (Nextcloud's calendar app), for the links.
    pub calendar_url: Option<String>,
    /// The account of each person by the name the wiki shows for them (and by their login),
    /// from the last read of the directory: their chosen one, else the house account; None
    /// for a name two people share.
    accounts_by_name: std::sync::Mutex<HashMap<String, Option<OwnedUserId>>>,
    pub(crate) state: Mutex<State>,
    state_path: PathBuf,
    /// The plenums whose area was checked since the start: a restart (a deploy with a new
    /// name, say) checks every area at once, not only an hour after the last check.
    areas_checked: std::sync::Mutex<HashSet<String>>,
}

/// Who sent a command.
pub struct Sender {
    pub id: OwnedUserId,
    pub name: String,
}

/// What of a meeting's minutes goes into the room: its own setting, else the plenum's.
fn publication(p: &Plenum, m: &Meeting) -> Publication {
    m.publication.unwrap_or(p.publication)
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
        admins: impl Into<Admins>,
        state: State,
        state_path: PathBuf,
    ) -> Self {
        Self {
            plenums,
            tz,
            wiki,
            chat,
            pdf,
            admins: admins.into(),
            me: None,
            keep: HashSet::new(),
            ask_to_choose: false,
            choose_url: None,
            grace: membership::Grace::default(),
            stale_after: std::time::Duration::from_secs(3600),
            subtenant_groups: Vec::new(),
            stay_reminder_days: 14,
            snapshot: std::sync::Mutex::new(None),
            members_round: Mutex::new(()),
            calendar_url: None,
            accounts_by_name: std::sync::Mutex::new(HashMap::new()),
            state: Mutex::new(state),
            state_path,
            areas_checked: std::sync::Mutex::new(HashSet::new()),
        }
    }

    pub fn rooms(&self) -> HashSet<OwnedRoomId> {
        self.plenums.iter().map(|p| p.room.clone()).collect()
    }

    fn today(&self, now: DateTime<Utc>) -> NaiveDate {
        now.with_timezone(&self.tz).date_naive()
    }

    /// The state on disk. Every step saves before its side effect (the document's ID, the
    /// message's transaction ID): a step whose save fails must not go on, or a restart would
    /// do it again.
    async fn save(&self, st: &State) -> Result<()> {
        st.save(&self.state_path).await.context("saving the state")
    }

    /// For the places that cannot stop: logged, tried again at the next save.
    async fn save_or_log(&self, st: &State) {
        if let Err(e) = self.save(st).await {
            error!("{e:#}");
        }
    }

    /// Everything that is due. Errors are logged per meeting; the others go on.
    pub async fn tick(&self, now: DateTime<Utc>) {
        let mut st = self.state.lock().await;
        for base in &self.plenums {
            let p = &self.effective(&st, base);
            self.follow_calendar(&mut st, p, now).await;
            self.tick_plenum(&mut st, p, now).await;
            if let Err(e) = self.carry_topics_on(&mut st, p).await {
                warn!(plenum = %p.key, "Topics onto the next page (again at the next tick): {e:#}");
            }
            if let Err(e) = self.tick_area(&mut st, p, now).await {
                warn!(plenum = %p.key, "Wiki area: {e:#}");
            }
            st.plenums.entry(p.key.clone()).or_default().prune();
            if let Some(room) = st.settings.plenums.get_mut(&p.key) {
                room.prune(now);
            }
        }
        self.save_or_log(&st).await;
    }

    /// The plenum with its room's own settings applied (plenum_settings.rs).
    pub(crate) fn effective(&self, st: &State, base: &Plenum) -> Plenum {
        match st.settings.plenums.get(&base.key) {
            Some(room) => plenum_settings::apply(base, &room.values),
            None => base.clone(),
        }
    }

    /// The area in the wiki: checked every [`AREA_EVERY`] (and right after the start), the
    /// overview written when its text changed.
    async fn tick_area(&self, st: &mut State, p: &Plenum, now: DateTime<Utc>) -> Result<()> {
        let due = !self.areas_checked.lock().unwrap().contains(&p.key)
            || st
                .plenums
                .get(&p.key)
                .and_then(|ps| ps.area_checked)
                .is_none_or(|at| now - at >= AREA_EVERY);
        if due {
            self.setup(st, p).await?;
            st.plenums.entry(p.key.clone()).or_default().area_checked = Some(now);
            self.areas_checked.lock().unwrap().insert(p.key.clone());
            self.save(st).await?;
        }
        let overview = self.write_overview(st, p, now).await;
        if let Err(e) = self.write_pins(st, p).await {
            warn!(plenum = %p.key, "Pinning the open pages (again at the next tick): {e:#}");
        }
        overview
    }

    /// The plenum's open pages pinned in its collection (Outline shows pins above the
    /// documents, on every tab), each unpinned once closed or cancelled; only when they
    /// changed. Pins of other documents (people's) are left alone.
    async fn write_pins(&self, st: &mut State, p: &Plenum) -> Result<()> {
        let ps = st.plenums.entry(p.key.clone()).or_default();
        let Some(collection) = ps.collection_id.clone() else {
            return Ok(());
        };
        let mut open: Vec<(MeetingKey, String)> = ps
            .meetings
            .values()
            .filter(|m| m.created && !m.close.done && !m.skipped && !m.cancelled)
            .filter_map(|m| Some((m.start?, m.doc_id.clone())))
            .collect();
        open.sort();
        let wanted: Vec<String> = open.into_iter().map(|(_, d)| d).collect();
        if ps.pinned.as_ref() == Some(&wanted) {
            return Ok(());
        }
        // only what the bot pinned itself (Outline does not say who pinned): a closed page
        // someone pinned stays
        let pages: HashSet<String> = ps.pinned.iter().flatten().cloned().collect();
        let allowed = match self.wiki.pins(&collection).await? {
            None => false,
            Some(pins) => {
                let mut allowed = true;
                for (pin, doc) in &pins {
                    if pages.contains(doc) && !wanted.contains(doc) {
                        allowed &= self.wiki.unpin(pin).await?;
                    }
                }
                for doc in &wanted {
                    if !pins.iter().any(|(_, d)| d == doc) {
                        // false also for a page deleted in the wiki (Outline: 403)
                        allowed &= self.wiki.pin(&collection, doc).await?;
                    }
                }
                allowed
            }
        };
        if !allowed {
            warn!(plenum = %p.key, "The wiki did not let the bot pin or unpin a page (its key needs pins.*, it must be an admin of the collection, the page must exist): tried again when the open pages change");
        }
        st.plenums.entry(p.key.clone()).or_default().pinned = Some(wanted);
        self.save(st).await
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
        let next: Vec<(MeetingKey, Option<String>, Publication)> = next
            .into_iter()
            .map(|(id, start)| {
                let m = ps.meetings.get(&id);
                let link = m.filter(|m| m.created).map(|m| self.link_of(m));
                let shared = m.map_or(p.publication, |m| publication(p, m));
                (start, link, shared)
            })
            .collect();
        let example = url("example");
        let links = protocol::OverviewLinks {
            rules: &rules,
            decisions: &decisions,
            protocols: &protocols,
            template: &template,
            example: example.as_deref(),
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
        self.save(st).await?;
        Ok(())
    }

    /// The calendar as last read: an occurrence a meeting is known by keeps it, a moved one is renamed and
    /// announced again, one gone from the calendar before it began is cancelled, one that
    /// comes back is not any more.
    async fn follow_calendar(&self, st: &mut State, p: &Plenum, now: DateTime<Utc>) {
        let tz = self.tz;
        let ps = st.plenums.entry(p.key.clone()).or_default();
        if ps.calendar_read.is_none() {
            return;
        }
        let occurrences = ps.occurrences.clone();
        let ids: HashSet<&str> = occurrences.iter().map(|o| o.id.as_str()).collect();
        // A series changed "this and all future" in Nextcloud's app becomes a new event (a new
        // UID, so new IDs): an open page whose ID left the calendar takes the one occurrence
        // new to the bot on its day, and is moved rather than cancelled. (Read between the two
        // writes of such a change, it is cancelled first and back on at the next read.)
        let gone: Vec<(String, MeetingKey)> = ps
            .meetings
            .iter()
            .filter(|(id, m)| !ids.contains(id.as_str()) && m.created && !m.close.done)
            .filter_map(|(id, m)| Some((id.clone(), m.start?)))
            .filter(|(_, start)| start.start(tz) > now)
            .collect();
        for (id, start) in gone {
            let new: Vec<&Occurrence> = occurrences
                .iter()
                .filter(|o| o.start.date() == start.date())
                // an ID the bot has not seen before (not another meeting of that day)
                .filter(|o| !ps.meetings.contains_key(&o.id))
                .collect();
            if let [o] = new[..] {
                info!(plenum = %p.key, from = %id, to = %o.id, "The calendar gave the meeting another ID: its page stays");
                let m = ps.meetings.remove(&id).expect("listed above");
                ps.meetings.insert(o.id.clone(), m);
            }
        }
        // What changed is recorded here, without a call that could fail: the notices and the
        // titles below follow from it and are tried again at every tick until they went out.
        for occ in &occurrences {
            let Some(m) = ps.meetings.get_mut(&occ.id) else {
                continue;
            };
            let to_come = occ.start.start(tz) > now;
            if m.cancelled && !m.close.done {
                info!(plenum = %p.key, meeting = %occ.id, "Back in the calendar");
                m.cancelled = false;
                m.skipped = false;
                // The room was told it is cancelled: it hears that it is on again.
                if m.cancel_notice.done && to_come {
                    m.announce = Default::default();
                    m.remind = Default::default();
                    m.announce_again = true;
                }
                m.cancel_notice = Default::default();
            }
            if let Some(old) = m.start {
                if old != occ.start && m.created && !m.close.done {
                    info!(plenum = %p.key, meeting = %occ.id, from = %old, to = %occ.start, "Moved in the calendar");
                    // Still to come: announced again (at once if the room knew the old date,
                    // saying from when) and reminded of again.
                    if to_come {
                        if m.announce.done {
                            m.moved_from = Some(m.moved_from.unwrap_or(old));
                            m.announce_again = true;
                        }
                        m.announce = Default::default();
                        m.remind = Default::default();
                    }
                }
            }
            m.start = Some(occ.start);
            m.end = occ.end;
        }
        for (id, m) in ps.meetings.iter_mut() {
            if !ids.contains(id.as_str())
                && m.created
                && !m.close.done
                && !m.cancelled
                && !m.skipped
                && m.start.is_some_and(|s| s.start(tz) > now)
            {
                info!(plenum = %p.key, meeting = %id, "Gone from the calendar: cancelled");
                m.cancelled = true;
            }
        }
        // occurrences not prepared yet that left the calendar: nothing to keep
        ps.meetings
            .retain(|id, m| m.created || m.close.done || m.skipped || ids.contains(id.as_str()));
        self.tell_cancellations(st, p).await;
        self.retitle(st, p).await;
    }

    /// A cancelled meeting the room was told about hears that it is cancelled, once.
    async fn tell_cancellations(&self, st: &mut State, p: &Plenum) {
        let due: Vec<String> = st.plenums[&p.key]
            .meetings
            .iter()
            .filter(|(_, m)| {
                m.cancelled
                    && m.created
                    && m.announce.done
                    && !m.cancel_notice.done
                    && !m.close.done
            })
            .map(|(id, _)| id.clone())
            .collect();
        for id in due {
            if let Err(e) = self.tell_cancellation(st, p, &id).await {
                warn!(plenum = %p.key, meeting = %id, "Telling the cancellation (again at the next tick): {e:#}");
            }
        }
    }

    async fn tell_cancellation(&self, st: &mut State, p: &Plenum, id: &str) -> Result<()> {
        let m = meeting(st, p, id);
        let txn = m.cancel_notice.txn();
        let start = m.start.context("a meeting without a start")?;
        let link = self.link_of(m);
        let shared = publication(p, m);
        self.save(st).await?;
        if sends(p.messages, Notice::Cancelled) {
            self.chat
                .send(
                    &p.room,
                    &protocol::cancellation(p, start, &link, shared),
                    Notice::Cancelled,
                    &txn,
                )
                .await?;
        } else {
            // quiet: the link message says it, no new message
            let text = protocol::cancelled_link(p, start, &link, shared);
            self.edit_link(st, p, id, &text).await?;
        }
        meeting(st, p, id).cancel_notice.done = true;
        self.save(st).await
    }

    /// The meeting's link message changed to `text` (an edit, each with its own transaction;
    /// nothing without a link message, e.g. one an older image posted).
    async fn edit_link(&self, st: &mut State, p: &Plenum, id: &str, text: &str) -> Result<()> {
        let m = meeting(st, p, id);
        let Some(link) = m.link.as_mut() else {
            return Ok(());
        };
        if link.text == text {
            return Ok(());
        }
        link.edits += 1;
        let (event, txn) = (
            link.event.clone(),
            format!("plenum-edit-{}-{}", link.event, link.edits),
        );
        // the count first: a retry after a crash sends another edit (the same text), never
        // an edit the server drops as a resend
        self.save(st).await?;
        self.chat.edit(&p.room, &event, text, &txn).await?;
        if let Some(link) = meeting(st, p, id).link.as_mut() {
            link.text = text.to_owned();
        }
        self.save(st).await
    }

    /// Every open protocol's title follows its meeting's date (a move); a rename that fails is
    /// tried again at the next tick.
    async fn retitle(&self, st: &mut State, p: &Plenum) {
        let todo: Vec<(String, String, String)> = st.plenums[&p.key]
            .meetings
            .iter()
            .filter(|(_, m)| m.created && !m.close.done)
            .filter_map(|(id, m)| {
                let title = self.free_title(st, p, id, m.start?);
                (m.title.as_deref() != Some(title.as_str()))
                    .then(|| (id.clone(), m.doc_id.clone(), title))
            })
            .collect();
        for (id, doc_id, title) in todo {
            match self.wiki.rename(&doc_id, &title).await {
                Ok(()) => {
                    info!(plenum = %p.key, meeting = %id, %title, "Protocol renamed");
                    meeting(st, p, &id).title = Some(title);
                }
                Err(e) => {
                    warn!(plenum = %p.key, meeting = %id, "Renaming the protocol (again at the next tick): {e:#}")
                }
            }
        }
    }

    /// The plenum's calendar was missing and made anew (main.rs): someone deleted it, maybe by
    /// mistake. Until it has events again, its empty reads change nothing (they would cancel
    /// every meeting).
    pub async fn calendar_remade(&self, key: &str) {
        let mut st = self.state.lock().await;
        st.plenums
            .entry(key.to_owned())
            .or_default()
            .calendar_remade = true;
        self.save_or_log(&st).await;
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
        if ps.calendar_remade {
            if occurrences.is_empty() {
                warn!(plenum = %key, "The calendar was made anew and is empty: the dates stay as they were until it has events again");
                return;
            }
            ps.calendar_remade = false;
        }
        // One meeting per start: an event entered twice (or a single event at a series' time)
        // is one meeting. An occurrence the bot keeps something of (a page, a publication set
        // for it) is never dropped; of the others at that time only the first stays when none
        // of them has anything.
        let mut occurrences = occurrences;
        let kept = |id: &str| {
            ps.meetings
                .get(id)
                .is_some_and(|m| m.created || m.close.done || m.publication.is_some())
        };
        occurrences
            .sort_by(|a, b| (a.start, !kept(&a.id), &a.id).cmp(&(b.start, !kept(&b.id), &b.id)));
        let mut last: Option<MeetingKey> = None;
        occurrences.retain(|o| {
            let first = last != Some(o.start);
            last = Some(o.start);
            first || kept(&o.id)
        });
        ps.occurrences = occurrences;
        ps.calendar_read = Some(now);
        self.save_or_log(&st).await;
    }

    async fn tick_plenum(&self, st: &mut State, p: &Plenum, now: DateTime<Utc>) {
        let today = self.today(now);
        let from = today
            - Duration::days(
                i64::from(p.duration_minutes / 1440 + 2) + i64::from(p.close_after_hours / 24 + 1),
            );
        let mut to = today + Duration::days(i64::from(p.prepare_days) + 1);
        let ps = st.plenums.entry(p.key.clone()).or_default();
        // with page_after the next meeting is in view however far away (its page opens after
        // the one before)
        if p.page_after_days.is_some() {
            if let Some(next) = (ps.occurrences.iter())
                .filter(|o| o.start.start(self.tz) > now)
                .map(|o| o.start.date())
                .min()
            {
                to = to.max(next);
            }
        }
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
                let start = m.start?;
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
        // a cancelled meeting's page, once its time is over: its topics move on, and that is all
        let over: Vec<String> = st.plenums[&p.key]
            .meetings
            .iter()
            .filter(|(_, m)| m.cancelled && m.created && !m.close.done)
            .filter(|(_, m)| m.start.is_some() && self.end_of(p, m) <= now)
            .map(|(id, _)| id.clone())
            .collect();
        for id in over {
            if let Err(e) = self.close_cancelled(st, p, &id).await {
                warn!(plenum = %p.key, meeting = %id, "A cancelled meeting's topics (again at the next tick): {e:#}");
            }
        }
    }

    /// A cancelled meeting's page after its time: all its topics wait for the next page;
    /// nothing else happens (no decisions list, PDF or message).
    async fn close_cancelled(&self, st: &mut State, p: &Plenum, id: &str) -> Result<()> {
        let (doc_id, start, link, shared) = {
            let m = meeting(st, p, id);
            let start = m.start.context("a meeting without a start")?;
            (m.doc_id.clone(), start, self.link_of(m), publication(p, m))
        };
        if let Some(doc) = self.wiki.doc(&doc_id).await? {
            // the meeting did not take place: every topic moves on
            let topics = page::topics(&doc.text);
            self.keep_topics(st, p, &topics, start, &link, shared);
        }
        let m = meeting(st, p, id);
        m.close.topics_read = true;
        m.close.done = true;
        info!(plenum = %p.key, meeting = %id, "Cancelled meeting over: its topics move on");
        self.save(st).await
    }

    /// `topics` of the meeting at `from` into the state, to go onto the next page (once: a
    /// repeated read replaces them).
    fn keep_topics(
        &self,
        st: &mut State,
        p: &Plenum,
        topics: &[page::Topic],
        from: MeetingKey,
        link: &str,
        shared: Publication,
    ) {
        let ps = st.plenums.entry(p.key.clone()).or_default();
        ps.open_topics.retain(|t| t.from != from);
        for t in topics {
            let block = page::carried_block(t, from.date(), link);
            ps.open_topics.push(CarriedTopic {
                heading: page::block_heading(&block).to_owned(),
                block,
                from,
                publication: shared,
            });
        }
    }

    /// The topics waiting in the state into the next page that is made (each into the first
    /// page after its meeting). One that the page has already (its heading) is dropped from the
    /// state, so it is only dropped once seen there, at a later tick than it was written; the
    /// meeting takes the stricter publication of the one it comes from.
    async fn carry_topics_on(&self, st: &mut State, p: &Plenum) -> Result<()> {
        let waiting = st
            .plenums
            .get(&p.key)
            .map(|ps| ps.open_topics.clone())
            .unwrap_or_default();
        if waiting.is_empty() {
            return Ok(());
        }
        let mut changed = false;
        for t in waiting {
            let ps = &st.plenums[&p.key];
            let next = ps
                .meetings
                .iter()
                .filter(|(_, m)| m.created && !m.close.done && !m.cancelled && !m.skipped)
                .filter(|(_, m)| m.start.is_some_and(|s| s > t.from))
                .min_by_key(|(id, m)| (m.start, id.as_str()))
                .map(|(id, m)| (id.clone(), m.doc_id.clone()));
            let Some((id, doc_id)) = next else {
                continue;
            };
            let Some(doc) = self.wiki.doc(&doc_id).await? else {
                continue;
            };
            let m = meeting(st, p, &id);
            if publication(p, m) < t.publication {
                m.publication = Some(t.publication);
                changed = true;
            }
            if page::has_heading(&doc.text, &t.heading) {
                let ps = st.plenums.get_mut(&p.key).expect("read above");
                ps.open_topics.retain(|o| o != &t);
                info!(plenum = %p.key, meeting = %id, "A carried topic is on the next page");
                changed = true;
            } else {
                self.add_block(&doc, &t.block).await?;
            }
        }
        if changed {
            self.save(st).await?;
        }
        Ok(())
    }

    /// A topic block into a page people may be writing in: in place of the topics' marker
    /// (above it), else of the old chat marker, else above the heading after the topics, else
    /// at the end (under a "Topics" heading if the page has none).
    async fn add_block(&self, doc: &Doc, block: &str) -> Result<()> {
        // at the level the page's topics are written at
        let block = &page::at_level(block, page::topics_level(&doc.text));
        for marker in [page::TOPIC_MARKER, protocol::TOP_MARKER] {
            if doc.text.contains(marker)
                && self
                    .wiki
                    .patch(&doc.id, marker, &format!("{block}\n\n{marker}"))
                    .await?
            {
                return Ok(());
            }
        }
        // above the heading that ends the topics ("Tasks"), so it is read as a topic
        if let Some(end) = page::end_of_topics(&doc.text) {
            if self
                .wiki
                .patch(&doc.id, end, &format!("{block}\n\n{end}"))
                .await?
            {
                return Ok(());
            }
        }
        // a page with topics up to its end: at the end; one without: under a "Topics" of its own
        let text = if page::has_topics(&doc.text) {
            format!("\n\n{block}")
        } else {
            format!("\n\n{}\n\n{block}", page::TOPICS_HEADING)
        };
        self.wiki.append(&doc.id, &text).await
    }

    /// The meeting before `start`: the latest earlier one that is neither cancelled nor
    /// skipped (docs/PLENUM-PAGES.md §4.1); its being over opens the next page.
    fn previous(&self, st: &State, p: &Plenum, start: MeetingKey) -> Option<Meeting> {
        let ps = st.plenums.get(&p.key)?;
        let mut earlier: Vec<Meeting> = ps
            .meetings
            .values()
            .filter(|m| m.start.is_some_and(|s| s < start))
            .cloned()
            .collect();
        for o in &ps.occurrences {
            if o.start < start && !ps.meetings.contains_key(&o.id) {
                earlier.push(Meeting {
                    start: Some(o.start),
                    end: o.end,
                    ..Default::default()
                });
            }
        }
        earlier
            .into_iter()
            .filter(|m| !m.cancelled && !m.skipped)
            .max_by_key(|m| m.start)
    }

    /// A meeting is over for the next page: its minutes closed, or past their closing time.
    fn is_over(&self, p: &Plenum, m: &Meeting, now: DateTime<Utc>) -> bool {
        m.close.done || now >= self.end_of(p, m) + Duration::hours(i64::from(p.close_after_hours))
    }

    /// The local midnight `days` after the day `m` ended (in local days: DST moves nothing).
    fn days_after(&self, p: &Plenum, m: &Meeting, days: u32) -> DateTime<Utc> {
        let day = self.end_of(p, m).with_timezone(&self.tz).date_naive()
            + Duration::days(i64::from(days));
        MeetingKey(day.and_time(chrono::NaiveTime::MIN)).start(self.tz)
    }

    /// When the meeting ends: the calendar's end, else its start and `duration_minutes`.
    fn end_of(&self, p: &Plenum, m: &Meeting) -> DateTime<Utc> {
        let start = m.start.map(|s| s.start(self.tz)).unwrap_or_default();
        match m.end {
            Some(end) if MeetingKey(end).start(self.tz) > start => MeetingKey(end).start(self.tz),
            _ => start + Duration::minutes(i64::from(p.duration_minutes)),
        }
    }

    /// The protocol's title for a meeting starting at `start`: "<Name> YYYY-MM-DD", with the
    /// time when another meeting that day has that title already.
    fn free_title(&self, st: &State, p: &Plenum, id: &str, start: MeetingKey) -> String {
        let title = protocol::title(p, start);
        let ps = st.plenums.get(&p.key);
        let taken = ps.is_some_and(|ps| {
            ps.meetings.iter().any(|(other, m)| {
                other != id
                    && m.created
                    && m.title
                        .as_deref()
                        .is_some_and(|t| t.trim_end_matches(CANCELLED) == title)
            })
        });
        let title = if taken {
            format!("{title} {}", start.0.format("%H:%M"))
        } else {
            title
        };
        let cancelled = ps
            .and_then(|ps| ps.meetings.get(id))
            .is_some_and(|m| m.cancelled);
        if cancelled {
            format!("{title}{CANCELLED}")
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
            if m.close.done || m.skipped || m.cancelled {
                return Ok(());
            }
            (
                m.start.context("a meeting without a start")?,
                self.end_of(p, m),
            )
        };
        let start = start_key.start(self.tz);
        // In local days, so a DST change in between moves nothing by an hour.
        let prepare_at =
            MeetingKey(start_key.0 - Duration::days(i64::from(p.prepare_days))).start(self.tz);
        {
            let m = &st.plenums[&p.key].meetings[key];
            // a day before, a page opens even while the one before is open (two meetings close
            // together)
            let soon = now + Duration::days(1) >= start;
            let previous = self.previous(st, p, start_key);
            let opens = match (p.page_after_days, &previous) {
                // page_after: `n` days after the meeting before, once it is over
                (Some(n), Some(prev)) => {
                    m.created
                        || soon
                        || (self.is_over(p, prev, now) && now >= self.days_after(p, prev, n))
                }
                // else (and for a first meeting): prepare_days before, once the one before is over
                _ => {
                    now >= prepare_at
                        && (m.created
                            || soon
                            || previous
                                .as_ref()
                                .is_none_or(|prev| self.is_over(p, prev, now)))
                }
            };
            let m = st
                .plenums
                .get_mut(&p.key)
                .and_then(|ps| ps.meetings.get_mut(key))
                .expect("due");
            if !opens {
                // The room knew another date (or that it was cancelled): told now, not only
                // when it is due.
                if m.created && m.announce_again && !m.announce.done {
                    self.announce(st, p, key).await?;
                }
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
        // collection would fail forever. The known one is renamed when the name changed.
        let known = st
            .plenums
            .get(&p.key)
            .and_then(|ps| ps.collection_id.clone());
        let collection = self
            .wiki
            .ensure_collection(
                known.as_deref(),
                &p.collection,
                p.access,
                p.group.as_deref(),
            )
            .await?;
        // Kept before the group is granted, which may fail: the next try finds it by ID.
        let ps = st.plenums.entry(p.key.clone()).or_default();
        if ps.collection_id.as_deref() != Some(collection.as_str()) {
            ps.pinned = None; // another collection: pinned anew
        }
        ps.collection_id = Some(collection.clone());
        self.save(st).await?;
        if let (Some(group), true) = (&p.group, p.access != Access::House) {
            if !self.wiki.grant_group(&collection, group).await? {
                warn!(plenum = %p.key, %group, "The wiki has no such group yet (outline-roles makes it): only its admins write here for now");
            }
        }
        // Each ID is kept as soon as it is known: when a later call fails (Outline's rate
        // limit, seen 2026-10-22), the next try must not make the same document again.
        let ps = st.plenums.entry(p.key.clone()).or_default();
        let rules = self
            .ensure_doc(
                &collection,
                &p.rules_title(),
                &protocol::rules_doc_text(p),
                &[],
                ps.rules_doc_id.as_deref(),
                &mut ps.written,
            )
            .await?;
        ps.urls.insert("rules".into(), rules.url);
        ps.rules_doc_id = Some(rules.id);
        let protocols = self
            .ensure_doc(
                &collection,
                &p.protocols_title(),
                &protocol::protocols_doc_text(p),
                &[],
                ps.protocols_doc_id.as_deref(),
                &mut ps.written,
            )
            .await?;
        ps.urls.insert("protocols".into(), protocols.url);
        ps.protocols_doc_id = Some(protocols.id);
        let example = self
            .ensure_doc(
                &collection,
                &p.example_title(),
                &protocol::example_doc_text(p),
                &former::examples(p),
                ps.example_doc_id.as_deref(),
                &mut ps.written,
            )
            .await?;
        ps.urls.insert("example".into(), example.url);
        ps.example_doc_id = Some(example.id);
        let overview = self
            .ensure_doc(
                &collection,
                &p.overview_title(),
                "",
                &[],
                ps.overview_doc_id.as_deref(),
                &mut ps.written,
            )
            .await?;
        if ps.overview_doc_id.as_deref() != Some(overview.id.as_str()) {
            ps.overview_text = None; // a new overview document: written again
        }
        ps.urls.insert("overview".into(), overview.url);
        ps.overview_doc_id = Some(overview.id);
        let template = self
            .ensure_doc(
                &collection,
                &p.template_title(),
                protocol::DEFAULT_TEMPLATE,
                &former::templates(),
                ps.template_doc_id.as_deref(),
                &mut ps.written,
            )
            .await?;
        ps.template_doc_id = Some(template.id.clone());
        ps.urls.insert("template".into(), template.url.clone());
        let decisions = self
            .ensure_doc(
                &collection,
                &p.decisions_title(),
                &protocol::decisions_doc_text(p),
                &[],
                ps.decisions_doc_id.as_deref(),
                &mut ps.written,
            )
            .await?;
        ps.decisions_doc_id = Some(decisions.id.clone());
        ps.urls.insert("decisions".into(), decisions.url.clone());
        Ok((collection, template.text, decisions))
    }

    /// The document `known` (if it still exists and is in the collection), else the one with
    /// `title`, else a new one. A known one with another title (the plenum's name or the
    /// bot's language changed) gets `title`. One that still has the bot's text gets `text`
    /// (a better explanation reaches every area): nobody changed it since the bot made it,
    /// it has the text the bot last wrote (`written`), or one the bot wrote in an earlier
    /// version (`earlier`). Empty `text`: the bot writes the document elsewhere.
    async fn ensure_doc(
        &self,
        collection: &str,
        title: &str,
        text: &str,
        earlier: &[String],
        known: Option<&str>,
        written: &mut BTreeMap<String, u64>,
    ) -> Result<Doc> {
        let mut found = None;
        if let Some(id) = known {
            if let Some(d) = self.wiki.doc(id).await? {
                if d.collection_id.as_deref().is_none_or(|c| c == collection) {
                    found = Some(d);
                }
            }
        }
        if found.is_none() {
            found = self.wiki.find_doc(collection, title).await?;
        }
        let Some(mut d) = found else {
            info!(%title, "Creating the document");
            let d = self
                .wiki
                .create_doc(
                    &uuid::Uuid::new_v4().to_string(),
                    collection,
                    None,
                    title,
                    text,
                )
                .await?;
            if !text.is_empty() {
                written.insert(d.id.clone(), former::fingerprint(&d.text));
            }
            return Ok(d);
        };
        if !text.is_empty() {
            let has = former::fingerprint(&d.text);
            let bots = d.untouched
                || written.get(&d.id) == Some(&has)
                || earlier.iter().any(|t| former::fingerprint(t) == has);
            if bots && has != former::fingerprint(text) {
                match self.wiki.replace(&d.id, text).await {
                    Ok(()) => {
                        info!(%title, "The bot's text of a document nobody changed, renewed");
                        d.text = text.to_owned();
                    }
                    // the old text stays (an archived document: Outline 403); tried again
                    // the next time
                    Err(e) => warn!(%title, "Renewing the bot's text: {e:#}"),
                }
            }
            if bots {
                written.insert(d.id.clone(), former::fingerprint(&d.text));
            }
        }
        if d.title != title {
            info!(from = %d.title, to = %title, "Renaming the document");
            self.wiki.rename(&d.id, title).await?;
            d.title = title.to_owned();
        }
        Ok(d)
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
            .start
            .context("a meeting without a start")?;
        // Worked out anew while there is no protocol: the meeting may have moved since a
        // failed try (the document is found by its ID, not its title).
        let title = self.free_title(st, p, key, start);
        let ps = st.plenums.get_mut(&p.key).expect("set up");
        let m = ps.meetings.get_mut(key).expect("due");
        m.title = Some(title.clone());
        let doc_id = m.doc_id.clone();
        // Saved before the call: after a crash the same ID is looked up instead of a second
        // protocol being created.
        self.save(st).await?;
        let tops = st.plenums[&p.key].pending_tops.clone();
        let doc = match self.wiki.doc(&doc_id).await? {
            // Created before a restart: items sent since then go in like any chat item.
            Some(d) => {
                for top in &tops {
                    if self.add_top_to(&doc_id, top).await?.is_none() {
                        anyhow::bail!("the protocol vanished while it was being prepared");
                    }
                }
                d
            }
            None => {
                let ps = &st.plenums[&p.key];
                let mut text = protocol::render_template(&template, p, start, &ps.open_tasks);
                let unknown = protocol::placeholders(&text);
                if !unknown.is_empty() {
                    warn!(plenum = %p.key, placeholders = %unknown.join(" "), "The template has placeholders the bot does not know: left as they are");
                }
                if page::has_topics(&text) {
                    // topics carried from earlier meetings first, then those from the chat
                    // each into the first page after its meeting: not when an earlier one is open
                    // (two meetings at one time: the one with the smaller ID comes first)
                    let open_before = |from: MeetingKey| {
                        ps.meetings.iter().any(|(id, m)| {
                            m.created
                                && !m.close.done
                                && !m.cancelled
                                && !m.skipped
                                && m.start
                                    .is_some_and(|s| s > from && (s, id.as_str()) < (start, key))
                        })
                    };
                    let mut stricter = None;
                    for t in ps
                        .open_topics
                        .iter()
                        .filter(|t| t.from < start && !open_before(t.from))
                    {
                        text = page::insert_block(&text, &t.block);
                        stricter = stricter.max(Some(t.publication));
                    }
                    for top in &tops {
                        text = page::insert_block(&text, &page::topic_block(top));
                    }
                    let m = meeting(st, p, key);
                    if stricter > Some(publication(p, m)) {
                        m.publication = stricter;
                    }
                } else {
                    for top in &tops {
                        text = protocol::insert_top(&text, &protocol::top_line(top));
                    }
                }
                let ps = &st.plenums[&p.key];
                let parent = ps.protocols_doc_id.clone();
                info!(plenum = %p.key, meeting = %key, "Creating the protocol");
                self.wiki
                    .create_doc(&doc_id, &collection, parent.as_deref(), &title, &text)
                    .await?
            }
        };
        let ps = st.plenums.get_mut(&p.key).expect("set up");
        ps.pending_tops.clear();
        let m = ps.meetings.get_mut(key).expect("inserted");
        m.created = true;
        m.url = Some(doc.url);
        info!(plenum = %p.key, meeting = %key, tops = tops.len(), "Protocol ready");
        self.save(st).await?;
        Ok(())
    }

    /// A chat item into a page made already: a topic block on a page written in topics, an
    /// agenda line on one of the old template. None: the page is gone.
    async fn add_top_to(&self, doc_id: &str, top: &Top) -> Result<Option<()>> {
        let Some(doc) = self.wiki.doc(doc_id).await? else {
            return Ok(None);
        };
        if !page::has_topics(&doc.text) {
            return self.add_lines(doc_id, &[protocol::top_line(top)]).await;
        }
        let block = page::topic_block(top);
        // after a restart the same item is not added twice (its heading and its author)
        let head: String = block.split("\n\n").take(2).collect::<Vec<_>>().join("\n\n");
        if !protocol::contains_line(&doc.text, &head) {
            self.add_block(&doc, &block).await?;
        }
        Ok(Some(()))
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
        let marker = doc
            .text
            .contains(protocol::TOP_MARKER)
            .then_some(protocol::TOP_MARKER);
        for line in lines {
            if protocol::contains_line(&doc.text, line) {
                continue;
            }
            if let Some(marker) = marker {
                if self
                    .wiki
                    .patch(doc_id, marker, &format!("{line}\n\n{marker}"))
                    .await?
                {
                    continue;
                }
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

    /// The link message when the page is open; after a move or a return: a new one (`all`) or
    /// the old one edited (`quiet`).
    async fn announce(&self, st: &mut State, p: &Plenum, key: &str) -> Result<()> {
        let m = meeting(st, p, key);
        if m.announce.done {
            return Ok(());
        }
        let link = self.link_of(m);
        let start = m.start.context("a meeting without a start")?;
        let shared = publication(p, m);
        let again = m.announce_again;
        let text = protocol::announcement(p, start, &link, m.moved_from, shared);
        let has_link = meeting(st, p, key).link.is_some();
        if again && !sends(p.messages, Notice::Moved) && has_link {
            self.edit_link(st, p, key, &text).await?;
        } else {
            // quiet, but the link message is an older image's (not to be edited): the link anew
            let notice = if again && sends(p.messages, Notice::Moved) {
                Notice::Moved
            } else {
                Notice::Link
            };
            let txn = meeting(st, p, key).announce.txn();
            self.save(st).await?;
            let event = self.chat.post(&p.room, &text, notice, &txn).await?;
            meeting(st, p, key).link = Some(LinkMessage {
                event,
                text,
                edits: 0,
            });
        }
        let m = meeting(st, p, key);
        m.announce.done = true;
        m.announce_again = false;
        m.moved_from = None;
        self.save(st).await?;
        Ok(())
    }

    async fn remind(&self, st: &mut State, p: &Plenum, key: &str, started: bool) -> Result<()> {
        let m = meeting(st, p, key);
        if m.remind.done {
            return Ok(());
        }
        if !sends(p.messages, Notice::Reminder) {
            // left out counts as done: a switch to `all` later sends nothing stale
            m.remind.done = true;
            return self.save(st).await;
        }
        let txn = m.remind.txn();
        let link = self.link_of(m);
        let start = m.start.context("a meeting without a start")?;
        let shared = publication(p, m);
        self.save(st).await?;
        self.chat
            .send(
                &p.room,
                &protocol::reminder(p, start, &link, started, shared),
                Notice::Reminder,
                &txn,
            )
            .await?;
        meeting(st, p, key).remind.done = true;
        self.save(st).await?;
        Ok(())
    }

    async fn close(&self, st: &mut State, p: &Plenum, key: &str, now: DateTime<Utc>) -> Result<()> {
        let (doc_id, link, start, title) = {
            let m = meeting(st, p, key);
            let start = m.start.context("a meeting without a start")?;
            let title = m.title.clone().unwrap_or_else(|| protocol::title(p, start));
            (m.doc_id.clone(), self.link_of(m), start, title)
        };
        let Some(doc) = self.wiki.doc(&doc_id).await? else {
            warn!(plenum = %p.key, meeting = %key, "The protocol was deleted; closed without it");
            meeting(st, p, key).close.done = true;
            self.save(st).await?;
            return Ok(());
        };
        let outcome = protocol::extract(&doc.text);
        // Read at every try: a change in the chat while a step fails reaches the next one.
        let shared = publication(p, meeting(st, p, key));
        // What the next protocol and the overview carry on with (each close sets it anew,
        // so a repeated close changes nothing).
        {
            let ps = st.plenums.entry(p.key.clone()).or_default();
            // Tasks of minutes kept out of the chat stay out of it when they come along.
            if shared != Publication::Full {
                for task in &outcome.tasks {
                    if !ps.withheld_tasks.contains(task) {
                        ps.withheld_tasks.push(task.clone());
                    }
                }
            }
            let date = start.date();
            let carried = |text: &String| Carried {
                text: text.clone(),
                date,
                link: link.clone(),
            };
            // The newest meeting's open tasks are the ones that go on.
            if ps.open_tasks_from.is_none_or(|from| from <= start) {
                ps.open_tasks = outcome.tasks.iter().map(carried).collect();
                ps.open_tasks_from = Some(start);
            }
            // By the protocol: two meetings on one day keep their decisions apart.
            ps.decisions.retain(|d| d.link != link);
            ps.decisions.extend(outcome.decisions.iter().map(carried));
            ps.decisions.sort_by_key(|d| d.date);
        }
        if !meeting(st, p, key).close.tasks_carried {
            self.carry_tasks_on(st, p, start).await?;
            meeting(st, p, key).close.tasks_carried = true;
            self.save(st).await?;
        }
        // the topics not reached wait for the next page (carry_topics_on puts them there)
        let topics = page::topics(&doc.text);
        if !meeting(st, p, key).close.topics_read {
            let carried: Vec<page::Topic> = topics
                .iter()
                .filter(|t| t.reading.carried())
                .cloned()
                .collect();
            self.keep_topics(st, p, &carried, start, &link, shared);
            meeting(st, p, key).close.topics_read = true;
            self.save(st).await?;
        }
        // the result box at the page's top, before the PDF; written, then seen at a later tick
        // (an API edit can be lost in a race with a browser's save)
        if !meeting(st, p, key).close.result_box {
            let c = &mut meeting(st, p, key).close;
            if page::has_box(&doc.text) || !doc.text.contains(page::RESULT_MARKER) {
                c.result_box = true;
            } else if c.box_attempts >= BOX_ATTEMPTS {
                warn!(plenum = %p.key, meeting = %key, "The result box did not stay in the page; closed without it");
                c.result_box = true;
            } else {
                c.box_attempts += 1;
                self.save(st).await?;
                let text = page::result_box(&outcome.decisions, &topics, &outcome.tasks);
                self.wiki
                    .patch(&doc_id, page::RESULT_MARKER, &format!("\n\n{text}\n\n"))
                    .await?;
                info!(plenum = %p.key, meeting = %key, "Result box written; the close goes on at the next tick");
                return Ok(());
            }
            self.save(st).await?;
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
                self.save(st).await?;
            }
        }

        // 2. the PDF, attached to the protocol and linked in it, then posted in the room
        if self.pdf.enabled && !meeting(st, p, key).close.pdf_failed {
            if let Err(e) = self.attach_pdf(st, p, key, &doc, &title, now).await {
                let c = &mut meeting(st, p, key).close;
                c.pdf_attempts += 1;
                if c.pdf_attempts < PDF_ATTEMPTS {
                    self.save(st).await?;
                    return Err(e.context("PDF"));
                }
                error!(plenum = %p.key, meeting = %key, "PDF given up after {} attempts: {e:#}", c.pdf_attempts);
                c.pdf_failed = true;
                self.save(st).await?;
            }
        }
        let c = &meeting(st, p, key).close;
        if self.pdf.post_to_room
            && shared == Publication::Full
            && sends(p.messages, Notice::Pdf)
            && c.pdf_url.is_some()
            && !c.pdf_room.done
            && !c.pdf_room_failed
        {
            if let Err(e) = self.post_pdf(st, p, key, &doc, &title, now).await {
                let c = &mut meeting(st, p, key).close;
                c.pdf_room_attempts += 1;
                if c.pdf_room_attempts < PDF_ATTEMPTS {
                    self.save(st).await?;
                    return Err(e.context("PDF into the room"));
                }
                error!(plenum = %p.key, meeting = %key, "Posting the PDF given up: {e:#}");
                c.pdf_room_failed = true;
                self.save(st).await?;
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
            self.save(st).await?;
            if !sends(p.messages, Notice::Summary) {
                // quiet: nothing after the meeting
            } else if shared == Publication::Full {
                let withheld = &st.plenums[&p.key].withheld_tasks;
                let shown = protocol::Outcome {
                    decisions: outcome.decisions.clone(),
                    tasks: (outcome.tasks.iter())
                        .filter(|t| protocol::may_share_task(t, withheld))
                        .cloned()
                        .collect(),
                };
                let text =
                    protocol::summary(p, start, &link, &shown, decisions_link.as_deref(), note);
                // who a task or a decision mentions in the wiki is mentioned in the chat too
                let (text, users) = protocol::for_matrix(&text, |name| self.account_of(name));
                self.chat
                    .send_mentioning(&p.room, &text, &users, Notice::Summary, &txn)
                    .await?;
            } else {
                let text = protocol::closed_note(p, start, &link, shared, note);
                self.chat
                    .send(&p.room, &text, Notice::Summary, &txn)
                    .await?;
            }
            meeting(st, p, key).close.summary.done = true;
        }
        meeting(st, p, key).close.done = true;
        info!(plenum = %p.key, meeting = %key, decisions = outcome.decisions.len(), publication = shared.word(), "Protocol closed");
        self.save(st).await?;
        Ok(())
    }

    /// The open tasks into the next protocol that is made already (meetings close together:
    /// it was prepared before this one closed). Only the tasks not in it yet, so a repeated
    /// close adds nothing.
    async fn carry_tasks_on(&self, st: &mut State, p: &Plenum, after: MeetingKey) -> Result<()> {
        let ps = &st.plenums[&p.key];
        if ps.open_tasks_from != Some(after) || ps.open_tasks.is_empty() {
            return Ok(());
        }
        let next = ps
            .meetings
            .iter()
            .filter(|(_, m)| m.created && !m.close.done && !m.cancelled && !m.skipped)
            .filter(|(_, m)| m.start.is_some_and(|s| s > after))
            .min_by_key(|(id, m)| (m.start, id.as_str()))
            .map(|(_, m)| m);
        let Some(next) = next else {
            return Ok(());
        };
        let Some(doc) = self.wiki.doc(&next.doc_id).await? else {
            return Ok(());
        };
        let block = protocol::open_tasks_block(&ps.open_tasks);
        let missing: Vec<&str> = block
            .lines()
            .filter(|l| l.starts_with("- [ ]") && !protocol::contains_line(&doc.text, l))
            .collect();
        if missing.is_empty() {
            return Ok(());
        }
        let heading = if protocol::contains_line(&doc.text, protocol::OPEN_TASKS_HEADING) {
            String::new()
        } else {
            format!("{}\n\n", protocol::OPEN_TASKS_HEADING)
        };
        self.wiki
            .append(&doc.id, &format!("\n\n{heading}{}", missing.join("\n")))
            .await?;
        info!(plenum = %p.key, tasks = missing.len(), "Open tasks into the next minutes, made already");
        Ok(())
    }

    fn render_pdf(&self, text: &str, title: &str, now: DateTime<Utc>) -> Result<Vec<u8>> {
        let stand = format!(
            "Minutes from the wiki, as of {}",
            now.with_timezone(&self.tz).format("%-d %b %Y %H:%M")
        );
        crate::pdf::render(
            title,
            &stand,
            text,
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
            let pdf = self.render_pdf(&doc.text, title, now)?;
            let url = self
                .wiki
                .attach(&doc.id, &file, "application/pdf", pdf)
                .await?;
            meeting(st, p, key).close.pdf_url = Some(url);
            self.save(st).await?;
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
                        &format!("\n\n---\n\nThe minutes as a PDF:\n\n[{file}]({url})"),
                    )
                    .await?;
            }
            meeting(st, p, key).close.pdf_linked = true;
            self.save(st).await?;
        }
        Ok(())
    }

    /// The PDF as a file in the plenum's room (once), without the tasks that came along from
    /// minutes kept out of the chat (the wiki's PDF has them).
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
        self.save(st).await?;
        let text = protocol::without_withheld(&doc.text, &st.plenums[&p.key].withheld_tasks);
        let pdf = self.render_pdf(&text, title, now)?;
        self.chat
            .send_file(&p.room, &format!("{title}.pdf"), pdf, Notice::Pdf, &txn)
            .await?;
        meeting(st, p, key).close.pdf_room.done = true;
        self.save(st).await?;
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
                .ok_or_else(|| format!("There is no plenum “{k}” here. {}", self.keys_hint(room))),
            None => match here.as_slice() {
                [one] => Ok(one),
                [] => Err("There is no plenum in this room.".to_owned()),
                _ => Err(format!(
                    "There are several plenums here; please add which one. {}",
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
            format!("Plenums here: {}", keys.join(", "))
        }
    }

    /// Closing the minutes after the meeting: the organizers (everyone in the room when there
    /// are none), admins.
    fn may_organize(&self, p: &Plenum, user: &UserId) -> bool {
        p.organizers.is_empty() || self.may_close_early(p, user)
    }

    /// Closing them while the meeting is still on: only the organizers named in the config and
    /// the admins (a close is final: what is decided afterwards misses the decision list).
    fn may_close_early(&self, p: &Plenum, user: &UserId) -> bool {
        p.organizers.contains(user) || self.admins.contains(user)
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
        const ORGANIZERS_ONLY: &str = "⛔ Only the organizers may do that here.";
        // the rooms' held removals: an admin in their direct chat with the bot only
        match (&cmd, scope) {
            (Command::Members | Command::ConfirmRemovals { .. }, Scope::Room) => {
                return "⛔ `!members` works in an admin's direct chat with me only.".to_owned()
            }
            (Command::Members, Scope::AdminDm) => return self.members_overview(now).await,
            (Command::ConfirmRemovals { room, digest }, Scope::AdminDm) => {
                return self.confirm_removals(room, digest, now).await
            }
            _ => {}
        }
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
                Ok(p) => self.request_close(&mut st, p, &sender.id, now),
                Err(e) => e,
            },
            Command::Publish { key, mode, when } => match self.pick(room, key.as_deref()) {
                Ok(p) => {
                    self.set_publication(&mut st, p, &sender.id, mode, when, now)
                        .await
                }
                Err(e) => e,
            },
            Command::Settings(key) => match self.pick_for_settings(room, key.as_deref(), scope) {
                Ok(p) => self.settings_text(&st, p, &sender.id, room),
                Err(e) => e,
            },
            Command::Set { key, name, value } => {
                match self.pick_for_settings(room, key.as_deref(), scope) {
                    Ok(p) => {
                        self.change_setting(&mut st, p, sender, room, &name, Some(&value), now)
                            .await
                    }
                    Err(e) => e,
                }
            }
            Command::Reset { key, name } => {
                match self.pick_for_settings(room, key.as_deref(), scope) {
                    Ok(p) => {
                        self.change_setting(&mut st, p, sender, room, &name, None, now)
                            .await
                    }
                    Err(e) => e,
                }
            }
            Command::Invalid(why) => why,
            Command::Members | Command::ConfirmRemovals { .. } => {
                unreachable!("answered above")
            }
        };
        self.save_or_log(&st).await;
        drop(st);
        answer
    }

    /// The plenum a settings command is for: in its room by the key or alone, in an admin's
    /// direct chat by its key.
    fn pick_for_settings<'a>(
        &'a self,
        room: &RoomId,
        key: Option<&str>,
        scope: Scope,
    ) -> Result<&'a Plenum, String> {
        if scope == Scope::AdminDm && self.plenums_in(room).is_empty() {
            return match key {
                Some(k) => self
                    .plenums
                    .iter()
                    .find(|p| p.key == k)
                    .ok_or_else(|| format!("There is no plenum “{k}”.")),
                None => Err("Which plenum? Add its key: `!plenum settings house`.".to_owned()),
            };
        }
        self.pick(room, key)
    }

    fn key_hint(&self, p: &Plenum, room: &RoomId) -> String {
        if self.plenums_in(room).len() == 1 && p.room == room {
            String::new()
        } else {
            format!("{} ", p.key)
        }
    }

    /// `!plenum settings`.
    fn settings_text(&self, st: &State, p: &Plenum, user: &UserId, room: &RoomId) -> String {
        let empty = plenum_settings::RoomSettings::default();
        let own = st.settings.plenums.get(&p.key).unwrap_or(&empty);
        plenum_settings::describe(p, own, self.may_organize(p, user), &self.key_hint(p, room))
    }

    /// `!plenum set <name> <value>` (`value` None: reset). The answer says what changed; a
    /// change from somewhere else is said in the plenum's room too (with `messages all`).
    #[allow(clippy::too_many_arguments)]
    async fn change_setting(
        &self,
        st: &mut State,
        p: &Plenum,
        sender: &Sender,
        room: &RoomId,
        name: &str,
        value: Option<&str>,
        now: DateTime<Utc>,
    ) -> String {
        if !plenum_settings::is_setting(name) {
            return plenum_settings::normalize(name, "").unwrap_err();
        }
        if !self.may_organize(p, &sender.id) {
            return "⛔ Only the plenum's organizers (or a bot admin) may change its settings."
                .to_owned();
        }
        if let Some(why) = plenum_settings::refused(p) {
            return why;
        }
        let value = match value {
            Some(raw) => match plenum_settings::normalize(name, raw) {
                Ok(v) => Some(v),
                Err(why) => return why,
            },
            None => None,
        };
        let admin = self.admins.contains(&sender.id);
        let own = st.settings.plenums.entry(p.key.clone()).or_default();
        if !own.may_change(sender.id.as_str(), admin, now) {
            return format!(
                "⏳ That is enough changes for today ({} per person, {} per plenum); more tomorrow, please.",
                plenum_settings::PER_PERSON_A_DAY,
                plenum_settings::PER_PLENUM_A_DAY
            );
        }
        let before_values = own.values.clone();
        let before = plenum_settings::current(p, &own.values, name);
        match &value {
            Some(v) => {
                own.values.insert(name.to_owned(), v.clone());
            }
            None => {
                own.values.remove(name);
            }
        }
        let after = plenum_settings::current(p, &own.values, name);
        if before == after {
            own.values = before_values;
            return format!("`{name}` is **{after}** already.");
        }
        own.changes.push(plenum_settings::Change {
            by: sender.name.clone(),
            by_user: sender.id.to_string(),
            name: name.to_owned(),
            from: before.clone(),
            to: after.clone(),
            at: now,
        });
        if let Err(e) = self.save(st).await {
            error!("{e:#}");
            // undone: what is not saved must not seem changed
            let own = st.settings.plenums.entry(p.key.clone()).or_default();
            own.changes.pop();
            own.values = before_values;
            // settings.json may have been written before state.json failed
            if let Err(e) = self.save(st).await {
                error!("Undoing a setting that could not be saved: {e:#}");
            }
            return "⚠️ The setting could not be saved just now. Please try again.".to_owned();
        }
        info!(plenum = %p.key, setting = name, from = %before, to = %after, "Setting changed");
        let said = format!(
            "{}{}. `!plenum settings` shows them all.",
            plenum_settings::change_text(p, &sender.name, name, &before, &after),
            if value.is_none() {
                " (the default again)"
            } else {
                ""
            }
        );
        // changed elsewhere (an admin's direct chat): the room hears it, unless it is quiet
        // (before the change or after it)
        let now_p = self.effective(st, p);
        let was_p = plenum_settings::apply(p, &before_values);
        if room != p.room
            && sends(now_p.messages, Notice::Setting)
            && sends(was_p.messages, Notice::Setting)
        {
            let txn = format!("plenum-setting-{}", uuid::Uuid::new_v4());
            if let Err(e) = self.chat.send(&p.room, &said, Notice::Setting, &txn).await {
                warn!(plenum = %p.key, "Telling the room about a setting: {e:#}");
            }
        }
        said
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

    /// The meeting `!minutes done` means: the latest one that has started and is open.
    fn current_meeting(&self, st: &State, p: &Plenum, now: DateTime<Utc>) -> Option<String> {
        let ps = st.plenums.get(&p.key)?;
        ps.meetings
            .iter()
            .filter(|(_, m)| m.created && !m.close.done && !m.skipped && !m.cancelled)
            .filter_map(|(id, m)| Some((m.start?, id)))
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
            .filter(|(_, m)| self.end_of(p, m) > now)
            .filter_map(|(id, m)| Some((m.start?, id.clone())))
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
            return format!("No plenum “{}”.", key.unwrap_or(""));
        }
        let mut out = Vec::new();
        for p in list {
            let mut line = format!("📋 **{}** (`{}`)", p.name, p.key);
            match self.next_meeting(st, p, now) {
                Some((id, start)) => {
                    line.push_str(&format!(
                        "\nNext meeting: {}",
                        schedule::describe_meeting(start)
                    ));
                    let m = st.plenums[&p.key].meetings.get(&id);
                    let shared = m.map_or(p.publication, |m| publication(p, m));
                    match m.filter(|m| m.created) {
                        Some(_) if shared == Publication::Confidential => {
                            line.push_str("\nMinutes: confidential, in the wiki")
                        }
                        Some(m) => line.push_str(&format!("\nMinutes: {}", self.link_of(m))),
                        None => {}
                    }
                    line.push_str(&format!(
                        "\nIn the chat: {}",
                        protocol::publication_text(shared)
                    ));
                }
                None => line.push_str("\nNo meeting in the calendar."),
            }
            match &self.calendar_url {
                Some(url) => line.push_str(&format!(
                    "\nMeetings: calendar “{}” in Nextcloud ({url})",
                    p.name
                )),
                None => line.push_str(&format!("\nMeetings: calendar “{}” in Nextcloud", p.name)),
            }
            if let Some(u) = st.plenums[&p.key].urls.get("overview") {
                line.push_str(&format!("\nOverview: {}", self.wiki.link(u)));
            }
            match st.plenums[&p.key].pending_tops.len() {
                0 => {}
                1 => line.push_str("\n1 topic from the chat waits for the next page."),
                n => line.push_str(&format!(
                    "\n{n} topics from the chat wait for the next page."
                )),
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
        // The open protocol of the next meeting, if it exists already.
        let target = self.next_meeting(st, p, now).and_then(|(id, _)| {
            st.plenums[&p.key]
                .meetings
                .get(&id)
                .filter(|m| m.created)
                .map(|m| {
                    let link = match publication(p, m) {
                        Publication::Confidential => None,
                        _ => Some(self.link_of(m)),
                    };
                    (id.clone(), m.doc_id.clone(), link)
                })
        });
        let Some((key, doc_id, link)) = target else {
            st.plenums
                .get_mut(&p.key)
                .expect("next_meeting made it")
                .pending_tops
                .push(top);
            return match self.next_meeting(st, p, now) {
                Some((_, start)) => format!(
                    "📝 Noted for {} on {}. The topic goes onto its page as soon as the page is open.",
                    p.name,
                    schedule::describe_meeting(start)
                ),
                None => format!("📝 Noted. {} has no meeting in the calendar yet; the topic goes onto the next page.", p.name),
            };
        };
        match self.add_top_to(&doc_id, &top).await {
            Ok(Some(())) => {
                info!(plenum = %p.key, meeting = %key, "Agenda item added");
                match link {
                    Some(link) => format!("✅ On the page: {link}"),
                    None => "✅ On the page. The meeting is confidential: its page is in the \
                             wiki, not here."
                        .to_owned(),
                }
            }
            Ok(None) => "⚠️ The minutes were deleted. Please ask the admins.".to_owned(),
            Err(e) => {
                warn!(plenum = %p.key, "Agenda item: {e:#}");
                "⚠️ The item could not be added just now. Please write it into the minutes directly, or try again in a moment.".to_owned()
            }
        }
    }

    fn request_close(
        &self,
        st: &mut State,
        p: &Plenum,
        user: &UserId,
        now: DateTime<Utc>,
    ) -> String {
        let Some(key) = self.current_meeting(st, p, now) else {
            return format!(
                "{} has no open minutes of a meeting that has started.",
                p.name
            );
        };
        let end = self.end_of(p, meeting(st, p, &key));
        if now < end && !self.may_close_early(p, user) {
            return format!(
                "⏳ The meeting is on until {}; the minutes close by themselves after it. \
                 Before that only the organizers or the admins close them.",
                end.with_timezone(&self.tz).format("%H:%M")
            );
        }
        let start = meeting(st, p, &key).start;
        meeting(st, p, &key).close.requested = true;
        let shared = publication(p, meeting(st, p, &key));
        let here = match (
            sends(self.effective(st, p).messages, Notice::Summary),
            shared,
        ) {
            (true, Publication::Full) => ", a summary here",
            (true, Publication::Link) => ", the link here",
            _ => "",
        };
        format!(
            "👍 The page of {} is closed in a moment: its result at the top, the decisions into the list, the PDF{here}.",
            start.map(schedule::describe_meeting).unwrap_or_default()
        )
    }

    /// The meetings not closed yet: the open protocols of meetings that began, and the
    /// calendar's dates to come, by their start.
    fn open_meetings(
        &self,
        st: &mut State,
        p: &Plenum,
        now: DateTime<Utc>,
    ) -> Vec<(String, MeetingKey)> {
        let mut list = self.upcoming(st, p, now, usize::MAX);
        for (id, m) in &st.plenums[&p.key].meetings {
            let open = m.created && !m.close.done && !m.skipped && !m.cancelled;
            if let (true, Some(start)) = (open, m.start) {
                if !list.iter().any(|(other, _)| other == id) {
                    list.push((id.clone(), start));
                }
            }
        }
        list.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.cmp(&b.0)));
        list
    }

    /// `!minutes full|link|confidential [day]`: what of a meeting's minutes goes into the room
    /// from now on. Without a day it is the meeting whose minutes are open (it began and is
    /// not closed yet), else the next one. Everyone may share less; sharing more again is for
    /// the organizers, the admins and who set it (a slip of theirs). Saved before the answer:
    /// what the answer says holds after a restart too.
    async fn set_publication(
        &self,
        st: &mut State,
        p: &Plenum,
        user: &UserId,
        mode: Publication,
        when: Option<When>,
        now: DateTime<Utc>,
    ) -> String {
        let open = self.open_meetings(st, p, now);
        let id = match when {
            None => match self.current_meeting(st, p, now) {
                Some(id) => id,
                None => match open.first() {
                    Some((id, _)) => id.clone(),
                    None => return format!("{} has no meeting in the calendar.", p.name),
                },
            },
            Some(when) => {
                let mut on_day: Vec<&(String, MeetingKey)> =
                    open.iter().filter(|(_, s)| when.matches(*s)).collect();
                // a day without its year: the next one (the calendar is read a year ahead)
                if let (None, Some((_, first))) = (when.year, on_day.first()) {
                    let day = first.date();
                    on_day.retain(|(_, s)| s.date() == day);
                }
                match on_day.as_slice() {
                    [(id, _)] => id.clone(),
                    [] => {
                        let closed = st.plenums[&p.key]
                            .meetings
                            .values()
                            .any(|m| m.close.done && m.start.is_some_and(|s| when.matches(s)));
                        return if closed {
                            format!(
                                "The minutes of {} on {} are closed already; the chat got \
                                 what it was set to then.",
                                p.name,
                                when.describe()
                            )
                        } else {
                            format!("{} has no open meeting on {}.", p.name, when.describe())
                        };
                    }
                    several => {
                        let times: Vec<String> = several
                            .iter()
                            .map(|(_, s)| s.0.format("%H:%M").to_string())
                            .collect();
                        return format!(
                            "{} has several meetings on {} ({}): please add the time, e.g. \
                             `!minutes {} {}.{}. {}`.",
                            p.name,
                            when.describe(),
                            times.join(", "),
                            mode.word(),
                            when.day,
                            when.month,
                            times[0]
                        );
                    }
                }
            }
        };
        // A date of the calendar not prepared yet gets its meeting now, to keep the setting.
        let ps = st.plenums.get_mut(&p.key).expect("open_meetings made it");
        if !ps.meetings.contains_key(&id) {
            let Some(occ) = ps.occurrences.iter().find(|o| o.id == id) else {
                return format!("{} has no meeting in the calendar.", p.name);
            };
            let m = Meeting {
                doc_id: uuid::Uuid::new_v4().to_string(),
                start: Some(occ.start),
                end: occ.end,
                ..Default::default()
            };
            ps.meetings.insert(id.clone(), m);
        }
        let m = meeting(st, p, &id);
        let date = m.start.map(schedule::describe_meeting).unwrap_or_default();
        let before = publication(p, m);
        if mode == before {
            return format!(
                "{} on {date}: already so. In the chat: {}.",
                p.name,
                protocol::publication_text(mode)
            );
        }
        let set_it = m.publication.is_some() && m.publication_by.as_deref() == Some(user.as_str());
        if mode < before && !self.may_close_early(p, user) && !set_it {
            return format!(
                "⛔ Only the organizers or the admins share more of {} on {date} again (or who \
                 set it to {}).",
                p.name,
                before.word()
            );
        }
        let (old, old_by) = (m.publication, m.publication_by.clone());
        m.publication = Some(mode);
        m.publication_by = Some(user.to_string());
        let posted = m.announce.done || m.remind.done;
        if let Err(e) = self.save(st).await {
            error!("{e:#}");
            let m = meeting(st, p, &id);
            m.publication = old;
            m.publication_by = old_by;
            return "⚠️ That could not be saved just now; nothing changed. Please try again in a \
                    moment."
                .to_owned();
        }
        info!(plenum = %p.key, meeting = %id, from = before.word(), to = mode.word(), by = %user, "Publication set");
        let icon = match mode {
            Publication::Full => "📋",
            Publication::Link => "🔗",
            Publication::Confidential => "🔒",
        };
        let mut answer = format!(
            "{icon} {} on {date}, from now on in the chat: {}. The minutes are made and closed \
             in the wiki as usual.",
            p.name,
            protocol::publication_text(mode)
        );
        if posted && mode == Publication::Confidential {
            answer.push_str(" The link to them is in the chat already (the announcement).");
        }
        if !sends(self.effective(st, p).messages, Notice::Summary) {
            answer.push_str(
                " (This plenum is quiet: only the link to each new page comes here anyway.)",
            );
        }
        answer
    }
}

/// A name as it is compared: lower case, whitespace collapsed.
fn name_key(name: &str) -> String {
    name.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

impl<W: Wiki, C: Chat> Engine<W, C> {
    /// The groups whose people the bot serves ([`config::managed_groups`]): the link codes and
    /// direct chats (main.rs `link_loop`) and the members' read (`members_loop`) use the same.
    pub fn managed_groups(&self) -> BTreeSet<String> {
        config::managed_groups(&self.plenums, &self.subtenant_groups)
    }

    /// Who lives here ([`config::living_groups`]): a stay of someone in none of them is over.
    fn living_groups(&self) -> BTreeSet<String> {
        config::living_groups(&self.plenums, &self.subtenant_groups)
    }

    /// Who the wiki's mentions are: every active person of a fresh read of the directory, by
    /// their shown name and by their login (a mention carries the name the wiki shows, which it
    /// takes from the house login), with their chosen account, else their house account
    /// ([`membership::address`]: a mention is no invitation, so an unchosen house account may
    /// be pinged). A name two people share names nobody.
    pub fn set_people(&self, people: &People) {
        let mut index: HashMap<String, Option<OwnedUserId>> = HashMap::new();
        for person in people.active() {
            let Some(account) = membership::address(people, person) else {
                continue;
            };
            let mut keys = vec![name_key(&person.username)];
            if !person.name.trim().is_empty() {
                keys.push(name_key(&person.name));
            }
            keys.dedup();
            for key in keys {
                index
                    .entry(key)
                    .and_modify(|known| {
                        if known.as_ref() != Some(&account) {
                            *known = None;
                        }
                    })
                    .or_insert_with(|| Some(account.clone()));
            }
        }
        *self.accounts_by_name.lock().unwrap() = index;
    }

    /// The account of the person the wiki shows as `name`, if exactly one.
    fn account_of(&self, name: &str) -> Option<OwnedUserId> {
        self.accounts_by_name
            .lock()
            .unwrap()
            .get(&name_key(name))
            .cloned()
            .flatten()
    }

    /// [`Self::sync_members`] after a read of the directory: when it failed, every room stays
    /// as it is. Err: why nothing changed (the read, or a guard), for the loop's health.
    pub async fn sync_members_after(
        &self,
        read: Result<Snapshot>,
        now: DateTime<Utc>,
    ) -> Result<(), String> {
        match read {
            Ok(snapshot) => {
                let snapshot = Arc::new(snapshot);
                // who the wiki's @ mentions are, for the summaries in the chat
                self.set_people(&snapshot.people);
                *self.snapshot.lock().unwrap() = Some(Arc::clone(&snapshot));
                let synced = self.sync_members(&snapshot, now).await;
                // not when a guard stopped the round: the read is not to be acted on
                if synced.is_ok() && self.me.is_some() {
                    self.remind_stays(&snapshot.people, now).await;
                }
                synced
            }
            Err(e) => {
                warn!("Members: the directory could not be read ({e:#}); nothing changed");
                Err(format!("the directory could not be read: {e:#}"))
            }
        }
    }

    /// The rooms the members' sync manages, each with its plenums: every plenum in the room
    /// has a group (a room with a plenum without one, the house chat, is left alone).
    fn managed_rooms(&self) -> Vec<(OwnedRoomId, Vec<&Plenum>)> {
        let mut rooms: Vec<OwnedRoomId> = self.plenums.iter().map(|p| p.room.clone()).collect();
        rooms.sort();
        rooms.dedup();
        rooms
            .into_iter()
            .map(|room| {
                let here = self.plenums_in(&room);
                (room, here)
            })
            .filter(|(_, here)| here.iter().all(|p| p.group.is_some()))
            .collect()
    }

    /// The rooms of plenums with a group follow it (mxbot-common `membership`): the chosen
    /// account of everyone in a plenum's group (and its `require_group`) is invited once and
    /// is the one that stays; their other accounts stay for the handover (`grace`, and after
    /// it while the chosen one is neither joined nor invited), all of them while they have not
    /// chosen; the accounts of nobody in it are removed, but for the
    /// kept accounts and the room's admins. A room shared by plenums follows all their
    /// groups. Nothing changes while the snapshot is stale or the house's group has nobody
    /// in it (Err: why), a room whose group the directory does not know is left as it is,
    /// and too many removals in one room wait for an admin (`!members confirm`).
    pub async fn sync_members(
        &self,
        snapshot: &Snapshot,
        now: DateTime<Utc>,
    ) -> Result<(), String> {
        let Some(me) = self.me.clone() else {
            return Ok(());
        };
        let _round = self.members_round.lock().await;
        let house = config::house_groups(&self.plenums);
        if let Some(halt) = membership::halt(
            snapshot,
            house.iter().map(String::as_str),
            self.stale_after,
            now.into(),
        ) {
            error!("Members: {halt}; no room is changed");
            return Err(halt.to_string());
        }
        // per person: the plenums they are wanted in without a usable account, and the rooms
        // a deactivated one is removed from
        let mut unreachable: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        let mut deactivated: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        // per person: the handover of their other accounts, over every room it runs in
        let mut handovers: BTreeMap<String, HandoverNews> = BTreeMap::new();
        for (room, here) in self.managed_rooms() {
            let rules: Vec<Rule> = here
                .iter()
                .filter_map(|p| room_rule(p, &house, &self.subtenant_groups))
                .collect();
            match self
                .sync_room(snapshot, &room, &here, &rules, &me, now)
                .await
            {
                Ok(Some(plan)) => {
                    let label = room_label(&here);
                    for (id, h) in &plan.handover {
                        let Ok(chosen) = OwnedUserId::try_from(h.chosen.as_str()) else {
                            continue;
                        };
                        if h.outgoing.is_empty() {
                            continue;
                        }
                        let until = h.until(&self.grace);
                        let news = handovers.entry(id.clone()).or_insert_with(|| HandoverNews {
                            chosen,
                            accounts: BTreeSet::new(),
                            rooms: BTreeSet::new(),
                            since: h.since,
                            until,
                            pending: false,
                            waiting: false,
                            absent: BTreeSet::new(),
                        });
                        match h.presence {
                            Presence::Joined => {}
                            Presence::Invited => {
                                news.pending = true;
                                news.waiting |= h.joined.is_none();
                            }
                            Presence::Absent => {
                                news.absent.insert(label.clone());
                            }
                        }
                        news.accounts.extend(h.outgoing.iter().cloned());
                        news.rooms.insert(label.clone());
                        news.since = news.since.min(h.since);
                        news.until = news.until.max(until);
                    }
                    for id in plan.unreachable {
                        unreachable.entry(id).or_default().insert(label.clone());
                    }
                    for person in plan.remove.into_iter().filter_map(|r| r.deactivated) {
                        deactivated.entry(person).or_default().insert(label.clone());
                    }
                }
                Ok(None) => {}
                Err(e) => warn!(%room, "Members: {e:#}"),
            }
        }
        self.tell_about_people(snapshot, &unreachable, &deactivated, &handovers, now)
            .await;
        Ok(())
    }

    /// One room's round; None when it is left alone (an unknown group).
    async fn sync_room(
        &self,
        snapshot: &Snapshot,
        room: &RoomId,
        here: &[&Plenum],
        rules: &[Rule],
        me: &UserId,
        now: DateTime<Utc>,
    ) -> Result<Option<membership::Plan>> {
        let current = self.chat.members(room).await?;
        let mut st = self.state.lock().await;
        let remembered = st.rooms.entry(room.to_string()).or_default();
        let plan = match membership::plan(
            snapshot,
            rules,
            &current,
            &self.keep,
            me,
            remembered,
            &self.grace,
            now,
        ) {
            Ok(plan) => plan,
            Err(skip) => {
                warn!(%room, "Members: {skip}; the room is left as it is");
                return Ok(None);
            }
        };
        remembered.forget_others(&plan.tolerated);
        remembered.handover = plan.handover.clone();
        for account in &plan.frozen {
            warn!(%room, %account, "Members: two people claim the account; neither invited nor removed");
        }
        let gate = membership::gate(remembered, &plan.remove, plan.humans, now);
        let groups: Vec<&str> = rules.iter().map(|r| r.group.as_str()).collect();
        let required: BTreeSet<&str> = rules
            .iter()
            .flat_map(|r| r.require.iter().map(String::as_str))
            .collect();
        let reason = if required.is_empty() {
            format!("not in the group {}", groups.join(", "))
        } else {
            format!(
                "not in the group {} or in none of {}",
                groups.join(", "),
                required.into_iter().collect::<Vec<_>>().join(", ")
            )
        };
        match gate {
            Gate::Go => {
                for r in &plan.remove {
                    let user = &r.account;
                    // what the person sees: an account they gave up gets its successor named
                    let (reason, why) = match &r.replaced_by {
                        Some(chosen) => (replaced_reason(chosen), "the handover is over"),
                        None => (reason.clone(), "not in the group"),
                    };
                    match self.chat.remove(room, user, &reason).await {
                        Ok(()) => info!(%room, %user, "Members: removed ({why})"),
                        Err(e) => warn!(%room, %user, "Members: removing failed: {e:#}"),
                    }
                }
            }
            Gate::Held { limit, tell } => {
                warn!(%room, removals = plan.remove.len(), limit, "Members: too many removals at once; held until an admin confirms");
                if tell {
                    // the hold's episode (digest, since when): the same plan held again later
                    // is a new message, not a resend the homeserver drops
                    let episode = remembered
                        .hold
                        .as_ref()
                        .map(membership::Hold::episode)
                        .unwrap_or_default();
                    let digest = remembered
                        .hold
                        .as_ref()
                        .map(membership::Hold::digest)
                        .unwrap_or_default();
                    let text = hold_notice(room, here, &plan, limit, &digest);
                    let key = format!("members-hold:{room}:{episode}");
                    match self.chat.tell_admins(&text, &key).await {
                        Ok(()) => {
                            if let Some(hold) = st
                                .rooms
                                .get_mut(room.as_str())
                                .and_then(|r| r.hold.as_mut())
                            {
                                hold.told = true;
                            }
                        }
                        Err(e) => warn!(%room, "Members: telling the admins failed: {e:#}"),
                    }
                }
            }
        }
        for user in &plan.invite {
            let remembered = st.rooms.entry(room.to_string()).or_default();
            match self.chat.invite(room, user).await {
                Ok(()) => {
                    info!(%room, %user, "Members: invited (in the group)");
                    remembered.record_invited(user);
                }
                Err(e) => {
                    // an account that does not exist (yet): tried again a day later
                    info!(%room, %user, "Members: invitation failed, again in a day: {e:#}");
                    remembered.record_failed(user, now);
                }
            }
        }
        let remembered = st.rooms.entry(room.to_string()).or_default();
        remembered.record_present(&current, &plan.tolerated);
        // the handovers as the invitations left them (an account invited now is invited)
        let mut plan = plan;
        plan.handover = remembered.handover.clone();
        self.save(&st).await?;
        Ok(Some(plan))
    }

    /// After the rooms: ask who has no usable account to choose one (`ask_to_choose`), tell
    /// who chose an account until when their other accounts stay (once per handover, at the
    /// chosen account), and tell the admins once about a choice that cannot be used, an
    /// account two people claim, who cannot be invited (the list, again when someone is added
    /// to it) and deactivated people still in a room. Every notice's key names its episode
    /// (the day it was found, what it is about, when a handover began), since the homeserver
    /// drops a key it has seen before.
    async fn tell_about_people(
        &self,
        snapshot: &Snapshot,
        unreachable: &BTreeMap<String, BTreeSet<String>>,
        deactivated: &BTreeMap<String, BTreeSet<String>>,
        handovers: &BTreeMap<String, HandoverNews>,
        now: DateTime<Utc>,
    ) {
        let people = &snapshot.people;
        let day = now.date_naive();
        let mut st = self.state.lock().await;
        st.people.tidy(people);
        for (account, names) in st.people.untold_conflicts(people) {
            warn!(%account, claimants = %names.join(", "), "Members: two people claim the account");
            let key = format!(
                "members-conflict:{account}:{}:{day}",
                membership::digest(&names)
            );
            match self
                .chat
                .tell_admins(&conflict_notice(&account, &names), &key)
                .await
            {
                Ok(()) => st.people.record_conflict(&account, &names),
                Err(e) => warn!(%account, "Members: telling the admins failed: {e:#}"),
            }
        }
        let lines: BTreeSet<String> = unreachable
            .iter()
            .filter_map(|(id, rooms)| {
                let person = people.by_id(id)?;
                Some(unreachable_line(person, people.contact(person), rooms))
            })
            .collect();
        if st.people.unreachable_is_news(&lines) {
            let text = unreachable_notice(&lines, self.ask_to_choose && self.choose_url.is_some());
            let key = format!("members-unreachable:{}:{day}", membership::digest(&lines));
            match self.chat.tell_admins(&text, &key).await {
                Ok(()) => st.people.record_unreachable(lines),
                Err(e) => warn!("Members: telling the admins failed: {e:#}"),
            }
        } else {
            // shrunk or the same: remembered, so who comes back is news again
            st.people.record_unreachable(lines);
        }
        for (id, rooms) in unreachable {
            let Some(person) = people.by_id(id) else {
                continue;
            };
            if let Contact::Invalid(why) = people.contact(person) {
                if !st.people.invalid.contains(id) {
                    let text = invalid_notice(person, why, self.choose_url.as_deref());
                    let chosen = match &person.choice {
                        Choice::Account(a) => a.to_string(),
                        _ => "malformed".to_owned(),
                    };
                    match self
                        .chat
                        .tell_admins(&text, &format!("members-invalid:{id}:{chosen}:{day}"))
                        .await
                    {
                        Ok(()) => {
                            st.people.invalid.insert(id.clone());
                        }
                        Err(e) => {
                            warn!(person = %person.username, "Members: telling the admins failed: {e:#}")
                        }
                    }
                }
            }
            let Some(url) = self.choose_url.as_deref().filter(|_| self.ask_to_choose) else {
                continue;
            };
            if !st.people.may_ask(id, now) {
                continue;
            }
            // every account of theirs (house account and links): where they read
            let to: Vec<&UserId> = people.accounts(person).collect();
            let times = st.people.asked.get(id).map_or(0, |a| a.times);
            let text = choose_message(person, rooms, url, self.me.as_deref());
            for account in &to {
                let key = format!("members-ask:{id}:{times}:{day}:{account}");
                match self.chat.message_person(account, &text, &key).await {
                    Ok(()) => {
                        info!(person = %person.username, %account, "Members: asked to choose an account")
                    }
                    Err(e) => {
                        info!(person = %person.username, %account, "Members: asking to choose failed: {e:#}")
                    }
                }
            }
            // counted when tried: an account that does not exist yet is not asked every round
            st.people.record_asked(id, now);
        }
        for (id, news) in handovers {
            if !st.people.handover_is_news(id, &news.chosen) {
                continue;
            }
            let Some(person) = people.by_id(id) else {
                continue;
            };
            let text = handover_message(
                person,
                news,
                self.grace.after_join,
                self.choose_url.as_deref(),
                self.tz,
            );
            let key = format!(
                "members-handover:{id}:{}:{}",
                news.chosen,
                news.since.to_rfc3339()
            );
            let to = &news.chosen;
            match self.chat.message_person(to, &text, &key).await {
                Ok(()) => {
                    info!(person = %person.username, account = %to, "Members: told about the handover")
                }
                Err(e) => {
                    // the admins see it in `!members`; the kick names the chosen account too
                    info!(person = %person.username, account = %to, "Members: telling about the handover failed: {e:#}")
                }
            }
            // told when tried, like asking: an account that cannot be written to is not tried
            // every round
            st.people.record_handover(id, &news.chosen);
        }
        for (id, rooms) in deactivated {
            if st.people.deactivated.contains(id) {
                continue;
            }
            let Some(person) = people.by_id(id) else {
                continue;
            };
            let text = deactivated_notice(person, rooms);
            let key = format!(
                "members-deactivated:{id}:{}:{day}",
                membership::digest(rooms)
            );
            match self.chat.tell_admins(&text, &key).await {
                Ok(()) => {
                    st.people.deactivated.insert(id.clone());
                }
                Err(e) => {
                    warn!(person = %person.username, "Members: telling the admins failed: {e:#}")
                }
            }
        }
        self.save_or_log(&st).await;
    }

    /// The admins hear of the end of a person's stay ([`crate::stay`]): before it, on the day,
    /// and when it is overdue, each once per person and date, in their direct chats with the bot
    /// (a notice that cannot be sent is tried again at the next read).
    async fn remind_stays(&self, people: &People, now: DateTime<Utc>) {
        let today = self.today(now);
        let mut st = self.state.lock().await;
        stay::tidy(&mut st.stays, people);
        let due = stay::due(
            people,
            &self.living_groups(),
            &st.stays,
            today,
            self.stay_reminder_days,
        );
        for reminder in &due {
            let text = stay::notice(reminder, today);
            match self.chat.tell_admins(&text, &reminder.key(today)).await {
                Ok(()) => {
                    info!(person = %reminder.person.username, until = %reminder.until, stage = reminder.stage.word(), "Members: told the admins about the end of a stay");
                    stay::record(&mut st.stays, reminder);
                }
                Err(e) => {
                    warn!(person = %reminder.person.username, "Members: telling the admins failed: {e:#}")
                }
            }
        }
        self.save_or_log(&st).await;
    }

    /// The managed room an admin means: a plenum key, a room ID or an alias.
    async fn managed_room(&self, target: &str) -> Option<OwnedRoomId> {
        let managed = self.managed_rooms();
        let room = if let Some(p) = self.plenums.iter().find(|p| p.key == target) {
            p.room.clone()
        } else if target.starts_with('#') {
            match self.chat.resolve_alias(target).await {
                Ok(room) => room,
                Err(e) => {
                    warn!(alias = %target, "Members: the alias could not be resolved: {e:#}");
                    return None;
                }
            }
        } else {
            OwnedRoomId::try_from(target).ok()?
        };
        managed
            .into_iter()
            .find(|(r, _)| *r == room)
            .map(|(r, _)| r)
    }

    /// `!members`: the held removals and the running handovers.
    async fn members_overview(&self, now: DateTime<Utc>) -> String {
        let st = self.state.lock().await;
        let mut lines = Vec::new();
        let mut handovers = Vec::new();
        for (room, here) in self.managed_rooms() {
            let remembered = st.rooms.get(room.as_str());
            for h in remembered
                .into_iter()
                .flat_map(|r| r.handover.values())
                .filter(|h| !h.outgoing.is_empty())
            {
                let until = h.until(&self.grace).with_timezone(&self.tz);
                let failed = remembered.is_some_and(|r| r.failed.contains_key(&h.chosen));
                let state = match h.presence {
                    Presence::Joined => String::new(),
                    Presence::Invited if h.joined.is_none() => " (invited, not joined yet)".into(),
                    Presence::Invited => " (invited again, not joined)".into(),
                    Presence::Absent => format!(
                        " ({}{}: the old {} until it is)",
                        if failed {
                            "its invitation failed"
                        } else if h.invited {
                            "declined or left after the handover's invitation"
                        } else {
                            "not in the room"
                        },
                        if h.overdue(&self.grace, now) {
                            ", past the end"
                        } else {
                            ""
                        },
                        if h.outgoing.len() == 1 {
                            "account stays"
                        } else {
                            "accounts stay"
                        },
                    ),
                };
                handovers.push(format!(
                    "- {}: {} → {}, until {}{state}",
                    room_label(&here),
                    h.outgoing.iter().cloned().collect::<Vec<_>>().join(", "),
                    h.chosen,
                    until.format("%-d %b %Y %H:%M"),
                ));
            }
            let Some(hold) = st.rooms.get(room.as_str()).and_then(|r| r.hold.as_ref()) else {
                continue;
            };
            let accounts: Vec<&str> = hold.accounts.iter().map(String::as_str).collect();
            lines.push(format!(
                "- {} (`{}`): {} removals held as `{}`{}: {}",
                room_label(&here),
                here[0].key,
                accounts.len(),
                hold.digest(),
                if hold.confirmed { ", confirmed" } else { "" },
                accounts.join(", ")
            ));
        }
        let (read, stays) = match &*self.snapshot.lock().unwrap() {
            Some(s) => (
                format!(
                    "The directory was last read {} min ago.",
                    s.age(now.into()).as_secs() / 60
                ),
                stay::overview(
                    &s.people,
                    &self.living_groups(),
                    self.today(now),
                    self.stay_reminder_days,
                ),
            ),
            None => (
                "The directory has not been read since the start.".to_owned(),
                Vec::new(),
            ),
        };
        let held = if lines.is_empty() {
            "No removals are held.".to_owned()
        } else {
            format!(
                "Held removals (`!members confirm <key> <digest>` carries out that plan once):\n{}",
                lines.join("\n")
            )
        };
        let handed = if handovers.is_empty() {
            String::new()
        } else {
            format!(
                "\n\nHandovers (other accounts of people who chose one: kept until {} days \
                 after the chosen account joined, {} days at most, and never while the chosen \
                 account is neither joined nor invited):\n{}",
                self.grace.after_join.num_days(),
                self.grace.max.num_days(),
                handovers.join("\n")
            )
        };
        let stays = if stays.is_empty() {
            String::new()
        } else {
            format!(
                "\n\nStays ending within {} days or ended (the directory's `stay_until`; \
                 nothing happens by itself):\n{}",
                self.stay_reminder_days,
                stays.join("\n")
            )
        };
        format!("{held}{handed}{stays}\n\n{read}")
    }

    /// `!members confirm <room> <digest>`: the room's held removals go once, at once if the
    /// directory's last read is fresh, and only if the plan held is the one whose digest the
    /// admin names (from the notice or `!members`) and is still the same then. Another digest:
    /// nothing is confirmed, the answer shows the plan held.
    async fn confirm_removals(&self, target: &str, digest: &str, now: DateTime<Utc>) -> String {
        let Some(room) = self.managed_room(target).await else {
            return format!(
                "⛔ No room whose members I keep matches “{target}” (a plenum key, room ID or alias)."
            );
        };
        let key = self
            .plenums_in(&room)
            .first()
            .map_or_else(|| room.to_string(), |p| p.key.clone());
        let held: BTreeSet<String> = {
            let mut st = self.state.lock().await;
            let Some(remembered) = st.rooms.get_mut(room.as_str()) else {
                return format!("Nothing is held in {room}.");
            };
            match remembered.confirm(digest) {
                Confirm::Nothing => return format!("Nothing is held in {room}."),
                Confirm::Other {
                    digest: held,
                    accounts,
                } => {
                    let named = if digest.is_empty() {
                        "no digest was given".to_owned()
                    } else {
                        format!("not “{digest}”")
                    };
                    return format!(
                        "⛔ The plan held in {room} is `{held}` ({named}): nothing was confirmed. \
                         It removes {n}: {list}.\n\nIf that is right: `!members confirm {key} {held}`.",
                        n = accounts.len(),
                        list = accounts.join(", "),
                    );
                }
                Confirm::Confirmed => remembered
                    .hold
                    .as_ref()
                    .map(|h| h.accounts.clone())
                    .unwrap_or_default(),
            }
        };
        {
            let st = self.state.lock().await;
            if let Err(e) = self.save(&st).await {
                error!("{e:#}");
                return "⚠️ The confirmation could not be saved; nothing changed.".to_owned();
            }
        }
        let snapshot = self.snapshot.lock().unwrap().clone();
        if let Some(snapshot) = snapshot {
            if let Err(why) = self.sync_members(&snapshot, now).await {
                return format!(
                    "Confirmed, but nothing changes now ({why}): the removals go at the next \
                     good read of the directory if the plan is still the same."
                );
            }
        }
        let hold = self
            .state
            .lock()
            .await
            .rooms
            .get(room.as_str())
            .and_then(|r| r.hold.clone());
        match hold {
            None => {
                // who of the plan is gone now (a kick that failed is in the log)
                let here: BTreeSet<String> = match self.chat.members(&room).await {
                    Ok(current) => current.iter().map(|m| m.id.to_string()).collect(),
                    Err(e) => {
                        warn!(%room, "Members: reading the room after the removals: {e:#}");
                        BTreeSet::new()
                    }
                };
                let (still, gone): (Vec<&String>, Vec<&String>) =
                    held.iter().partition(|a| here.contains(*a));
                let list =
                    |v: &[&String]| v.iter().map(|a| a.as_str()).collect::<Vec<_>>().join(", ");
                let mut answer = format!(
                    "✅ The held removals in {room} were carried out: removed {}.",
                    if gone.is_empty() {
                        "nobody".to_owned()
                    } else {
                        list(&gone)
                    }
                );
                if !still.is_empty() {
                    answer.push_str(&format!(
                        " Still there (removing failed, see the log): {}.",
                        list(&still)
                    ));
                }
                answer
            }
            Some(hold) if hold.confirmed => format!(
                "Confirmed: the removals in {room} go at the next good read of the directory, \
                 if the plan is still the same."
            ),
            Some(_) => format!(
                "The plan for {room} changed since it was held: nothing was removed. The admins \
                 hear of the new one."
            ),
        }
    }
}

/// The rule of a plenum's room (mxbot-common `membership::Rule`): its group, and who of it
/// counts: its `require_group`. A kitchen (access `members`) that requires the house's group
/// (`house`, the default) takes the members who live here, so `subtenant_groups` count too;
/// the house plenum's room (its own group, no requirement) and a private plenum's (finance)
/// never take them. None without a group.
pub(crate) fn room_rule(
    p: &Plenum,
    house: &BTreeSet<String>,
    subtenant_groups: &[String],
) -> Option<Rule> {
    let group = p.group.clone()?;
    let mut require: Vec<String> = p.require_group.iter().cloned().collect();
    let lives_here =
        p.access == Access::Members && p.require_group.as_ref().is_some_and(|r| house.contains(r));
    if lives_here {
        require.extend(subtenant_groups.iter().cloned());
    }
    require.sort();
    require.dedup();
    Some(Rule { group, require })
}

/// A room by its plenums' names.
fn room_label(here: &[&Plenum]) -> String {
    here.iter()
        .map(|p| p.name.as_str())
        .collect::<Vec<_>>()
        .join(" / ")
}

/// The reason a removed outgoing account sees (German: what the house speaks).
fn replaced_reason(chosen: &UserId) -> String {
    format!("Ersetzt durch dein gewähltes Konto {chosen} (Einstellungen → Matrix-Konten)")
}

/// A person's handover over every room it runs in, for their notice.
struct HandoverNews {
    /// The account they chose: the one written to.
    chosen: OwnedUserId,
    /// Their other accounts kept for now.
    accounts: BTreeSet<String>,
    /// The rooms (by their plenums' names) these are in.
    rooms: BTreeSet<String>,
    /// When the earliest of the rooms' handovers began: the notice's episode.
    since: DateTime<Utc>,
    /// When the last of them ends at the latest.
    until: DateTime<Utc>,
    /// The chosen account has an invitation it has not accepted in one of the rooms.
    pending: bool,
    /// …and it never joined there: the handover ends a while after it does.
    waiting: bool,
    /// The rooms the chosen account is neither joined nor invited to (its invitation failed,
    /// or it declined or left): the old accounts stay there until it is.
    absent: BTreeSet<String>,
}

fn hold_notice(
    room: &RoomId,
    here: &[&Plenum],
    plan: &membership::Plan,
    limit: usize,
    digest: &str,
) -> String {
    let accounts: Vec<String> = plan
        .remove
        .iter()
        .map(|r| match &r.replaced_by {
            Some(chosen) => format!("{} (handed over to {chosen})", r.account),
            None => r.account.to_string(),
        })
        .collect();
    format!(
        "⏸️ **Members: {n} removals held** in {label} ({room}): more than {limit} at once \
         ({humans} people in the room), so nobody is removed there until an admin confirms. \
         They would be: {list}.\n\n\
         Invitations still go out. If this is right: `!members confirm {key} {digest}` here \
         (this plan only: a changed one is held anew). If the directory is wrong, fix it \
         there: the hold ends by itself when the plan shrinks.",
        n = accounts.len(),
        label = room_label(here),
        humans = plan.humans,
        list = accounts.join(", "),
        key = here[0].key,
    )
}

/// An account two or more active people claim: it counts for nobody.
fn conflict_notice(account: &UserId, names: &[&str]) -> String {
    format!(
        "⚠️ **Members:** {account} is a Matrix account of {names} in the directory. It counts \
         for none of them: it is not invited, and stays only in rooms one of them belongs in. \
         Take it off the person whose account it is not (the link, or their chosen account).",
        names = names.join(" and "),
    )
}

/// One person who cannot be invited, as the admins' list shows them.
fn unreachable_line(
    person: &mxbot_common::directory::Person,
    contact: Contact,
    rooms: &BTreeSet<String>,
) -> String {
    let why = match contact {
        Contact::Invalid(_) => "their choice cannot be used",
        _ => "has not chosen",
    };
    format!(
        "{} ({why}): {}",
        person.username,
        rooms.iter().cloned().collect::<Vec<_>>().join(", ")
    )
}

/// Who is wanted in a room but cannot be invited, for the admins.
fn unreachable_notice(lines: &BTreeSet<String>, asking: bool) -> String {
    let asked = if asking {
        "I ask each of them to choose (`ask_to_choose`)."
    } else {
        "Nobody asks them: `[members] ask_to_choose` is off (house-infra matrix.yml, plenum-bot's \
         `members`), so tell them, or switch it on."
    };
    format!(
        "📭 **Members: {n} {people} cannot be invited** to the rooms they belong in until \
         they choose their Matrix account (where they are already, they stay). {asked}\n{list}",
        n = lines.len(),
        people = if lines.len() == 1 { "person" } else { "people" },
        list = lines
            .iter()
            .map(|l| format!("- {l}"))
            .collect::<Vec<_>>()
            .join("\n"),
    )
}

fn invalid_notice(
    person: &mxbot_common::directory::Person,
    why: Invalid,
    url: Option<&str>,
) -> String {
    let chosen = match &person.choice {
        Choice::Account(id) => id.to_string(),
        _ => "something that is no Matrix ID".to_owned(),
    };
    let why = match why {
        Invalid::Malformed => "it is no Matrix ID",
        Invalid::NotTheirs => "it is neither their house account nor one of their links",
        Invalid::Conflict => "another active person claims it too",
    };
    let fix = url.map_or(String::new(), |u| format!(" ({u})"));
    format!(
        "⚠️ **Members:** {user} chose {chosen} as their Matrix account, which cannot be used: \
         {why}. Nobody new is invited for them until they choose again{fix}.",
        user = person.username,
    )
}

fn deactivated_notice(
    person: &mxbot_common::directory::Person,
    rooms: &BTreeSet<String>,
) -> String {
    let accounts: Vec<&str> = person.accounts.iter().map(|a| a.as_str()).collect();
    format!(
        "🚪 **Members:** {user} is deactivated in the directory and goes out of {rooms} (held \
         removals once confirmed). Their Matrix account ({accounts}) is still active: logging \
         it out or suspending it is a human step (house-infra docs/OPERATIONS.md, offboarding).",
        user = person.username,
        rooms = rooms.iter().cloned().collect::<Vec<_>>().join(", "),
        accounts = accounts.join(", "),
    )
}

/// "a", "a und b", "a, b und c".
fn german_list(items: &[&str]) -> String {
    match items {
        [] => String::new(),
        [one] => (*one).to_owned(),
        [rest @ .., last] => format!("{} und {last}", rest.join(", ")),
    }
}

/// What a person who chose an account hears once per handover, at that account (German).
fn handover_message(
    person: &mxbot_common::directory::Person,
    news: &HandoverNews,
    after_join: Duration,
    url: Option<&str>,
    tz: Tz,
) -> String {
    let name = if person.name.trim().is_empty() {
        person.username.as_str()
    } else {
        person.name.trim()
    };
    let accounts: Vec<&str> = news.accounts.iter().map(String::as_str).collect();
    let rooms: Vec<&str> = news.rooms.iter().map(String::as_str).collect();
    let days = match after_join.num_days() {
        1 => "einen Tag".to_owned(),
        n => format!("{n} Tage"),
    };
    let fix = url.map_or(String::new(), |u| format!(" ({u})"));
    let join = match (news.pending, news.waiting) {
        (true, true) => format!(
            " Nimm die Einladung an {chosen} an: {days} danach ist der Wechsel dort fertig \
             (früher als dieses Datum).",
            chosen = news.chosen,
        ),
        (true, false) => format!(" Nimm die Einladung an {chosen} an.", chosen = news.chosen),
        _ => String::new(),
    };
    let absent: Vec<&str> = news.absent.iter().map(String::as_str).collect();
    let missing = if absent.is_empty() {
        String::new()
    } else {
        format!(
            " In {rooms} ist {chosen} gerade weder drin noch eingeladen (die Einladung ging \
             nicht durch oder wurde abgelehnt): dort {stay} {old}, bis {chosen} im Raum ist.",
            rooms = german_list(&absent),
            chosen = news.chosen,
            stay = if accounts.len() == 1 {
                "bleibt"
            } else {
                "bleiben"
            },
            old = german_list(&accounts),
        )
    };
    format!(
        "🔁 Hallo {name}! Dein Konto für die Hausräume ist jetzt {chosen}. {old} {stays} noch \
         bis spätestens {until} in {rooms}, danach nur noch {chosen}.{join}{missing}\n\n\
         Willst du ein anderes Konto nutzen, ändere es in den Einstellungen unter \
         „Matrix-Konten“{fix}.",
        chosen = news.chosen,
        old = german_list(&accounts),
        stays = if accounts.len() == 1 {
            "bleibt"
        } else {
            "bleiben"
        },
        until = news
            .until
            .with_timezone(&tz)
            .format("%-d.%-m.%Y, %H:%M Uhr"),
        rooms = german_list(&rooms),
    )
}

/// The request to choose an account (German: what the house speaks).
fn choose_message(
    person: &mxbot_common::directory::Person,
    rooms: &BTreeSet<String>,
    url: &str,
    me: Option<&UserId>,
) -> String {
    let name = if person.name.trim().is_empty() {
        person.username.as_str()
    } else {
        person.name.trim()
    };
    let house = person
        .house
        .as_ref()
        .map_or(String::new(), |h| format!(" ({h})"));
    let bot = me.map_or("mich".to_owned(), |m| format!("mich ({m})"));
    format!(
        "👋 Hallo {name}! Damit dich die Bots des Hauses in deine Räume einladen können \
         ({rooms}), wähle bitte einmal dein Matrix-Konto fürs Haus:\n\n{url}\n\n\
         Dort nimmst du dein Hauskonto{house} oder ein anderes Konto, das du vorher verknüpfst: \
         schreib dazu von diesem anderen Konto aus `!link {username}` an {bot} und gib den Code \
         dort ein.\n\n\
         Bis du gewählt hast, lade ich dich in keinen Raum neu ein; wo du schon bist, bleibst du.",
        rooms = rooms.iter().cloned().collect::<Vec<_>>().join(", "),
        username = person.username,
    )
}

pub fn help() -> String {
    "📋 **Plenum bot**\n\n\
     Each meeting has a page in the wiki: its topics, prepared there, and its minutes. Add a \
     topic as a heading under \"Topics\"; at the plenum its result goes after \"Result:\", \
     a decision as a line starting with \"Decision:\". A topic whose result stays empty \
     comes onto the next page.\n\n\
     `!plenum`: the next plenum and its page\n\
     `!top <topic>`: a topic onto the next page (the next lines: what it is about); or reply \
     to a message with `!top`\n\
     `!plenum settings`: what the bot posts here and when (`!plenum set <name> <value>`, \
     `!plenum reset <name>`)\n\
     `!minutes done`: close the page now (result, decisions, PDF)\n\
     `!minutes confidential`, `!minutes link`, `!minutes full`: nothing from the page in the \
     chat, only the link, or the summary, for the meeting that is on or comes next; for \
     another add its day: `!minutes confidential 14.11.`\n\n\
     The meetings are in the plenum's Nextcloud calendar: add, move or cancel them there, or \
     make a series; the bot follows it. With several plenums in a room, add the key: \
     `!top kitchen-floor-1 dishwasher`."
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
        /// changes of a document's text (replace, patch, append)
        pub writes: StdMutex<u32>,
        /// (pin, collection, document); `no_pins`: the key has no pins.*
        pub pins: StdMutex<Vec<(String, String, String)>>,
        pub pin_calls: StdMutex<u32>,
        pub no_pins: StdMutex<bool>,
        /// documents.update refused (an archived document)
        pub no_replace: StdMutex<bool>,
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
        async fn ensure_collection(
            &self,
            known: Option<&str>,
            name: &str,
            access: Access,
            _group: Option<&str>,
        ) -> Result<String> {
            self.check()?;
            let mut c = self.collections.lock().unwrap();
            let found = known
                .and_then(|k| c.iter().position(|(id, _)| id == k))
                .or_else(|| {
                    (access != Access::Private)
                        .then(|| c.iter().position(|(_, n)| n == name))
                        .flatten()
                });
            let id = match found {
                Some(i) => {
                    c[i].1 = name.to_owned();
                    c[i].0.clone()
                }
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
        async fn pins(&self, collection_id: &str) -> Result<Option<Vec<(String, String)>>> {
            self.check()?;
            *self.pin_calls.lock().unwrap() += 1;
            if *self.no_pins.lock().unwrap() {
                return Ok(None);
            }
            Ok(Some(
                self.pins
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|(_, c, _)| c == collection_id)
                    .map(|(p, _, d)| (p.clone(), d.clone()))
                    .collect(),
            ))
        }
        async fn pin(&self, collection_id: &str, doc_id: &str) -> Result<bool> {
            self.check()?;
            *self.pin_calls.lock().unwrap() += 1;
            if *self.no_pins.lock().unwrap() {
                return Ok(false);
            }
            let mut pins = self.pins.lock().unwrap();
            if !pins
                .iter()
                .any(|(_, c, d)| c == collection_id && d == doc_id)
            {
                let id = format!("pin-{}", pins.len());
                pins.push((id, collection_id.to_owned(), doc_id.to_owned()));
            }
            Ok(true)
        }
        async fn unpin(&self, pin_id: &str) -> Result<bool> {
            self.check()?;
            *self.pin_calls.lock().unwrap() += 1;
            if *self.no_pins.lock().unwrap() {
                return Ok(false);
            }
            self.pins.lock().unwrap().retain(|(p, _, _)| p != pin_id);
            Ok(true)
        }
        async fn rename(&self, id: &str, title: &str) -> Result<()> {
            self.check()?;
            let mut docs = self.docs.lock().unwrap();
            let (d, _) = docs
                .iter_mut()
                .find(|(d, _)| d.id == id)
                .context("no doc")?;
            d.title = title.to_owned();
            d.untouched = false;
            Ok(())
        }
        async fn replace(&self, id: &str, text: &str) -> Result<()> {
            self.check()?;
            if *self.no_replace.lock().unwrap() {
                anyhow::bail!("Outline 403 authorization_error");
            }
            let mut docs = self.docs.lock().unwrap();
            let (d, _) = docs
                .iter_mut()
                .find(|(d, _)| d.id == id)
                .context("no doc")?;
            d.text = text.to_owned();
            *self.writes.lock().unwrap() += 1;
            d.untouched = false;
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
                untouched: true,
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
            *self.writes.lock().unwrap() += 1;
            d.untouched = false;
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
            *self.writes.lock().unwrap() += 1;
            d.untouched = false;
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
        /// (txn, accounts) of every message that pinged someone
        pub pinged: StdMutex<Vec<(String, Vec<String>)>>,
        pub files: StdMutex<Vec<(String, String, usize)>>,
        pub fail: StdMutex<bool>,
        pub fail_files: StdMutex<bool>,
        /// (room, member) of every room
        pub members: StdMutex<Vec<(String, Member)>>,
        /// accounts an invitation fails for (they do not exist)
        pub unknown: StdMutex<Vec<String>>,
        /// accounts the rooms' invitations fail for, while a direct message reaches them (a
        /// room their server may not join, say)
        pub uninvitable: StdMutex<Vec<String>>,
        pub removed: StdMutex<Vec<(String, String, String)>>, // (room, user, reason)
        /// (key, text) of every notice to the admins
        pub admin_notes: StdMutex<Vec<(String, String)>>,
        /// (account, text, key) of every message to a person
        pub dms: StdMutex<Vec<(String, String, String)>>,
        /// (alias, room)
        pub aliases: StdMutex<Vec<(String, String)>>,
        /// What each message into a room was (by transaction ID), files too.
        pub notices: StdMutex<Vec<(String, Notice)>>,
        /// (room, event, text, txn) of each edit (a resend of a txn is dropped).
        pub edits: StdMutex<Vec<(String, String, String, String)>>,
    }

    impl FakeChat {
        /// The kinds of message that went into rooms, in order.
        pub fn kinds(&self) -> Vec<Notice> {
            self.notices
                .lock()
                .unwrap()
                .iter()
                .map(|(_, n)| *n)
                .collect()
        }

        /// The text an event shows now (its last edit).
        pub fn shown(&self, event: &str) -> Option<String> {
            let edits = self.edits.lock().unwrap();
            if let Some((_, _, t, _)) = edits.iter().rev().find(|(_, e, _, _)| e == event) {
                return Some(t.clone());
            }
            let n: usize = event.strip_prefix("$ev-")?.parse().ok()?;
            self.sent.lock().unwrap().get(n).map(|(_, t, _)| t.clone())
        }

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
        async fn send(
            &self,
            room: &RoomId,
            markdown: &str,
            notice: Notice,
            txn: &str,
        ) -> Result<()> {
            self.post(room, markdown, notice, txn).await.map(|_| ())
        }
        async fn post(
            &self,
            room: &RoomId,
            markdown: &str,
            notice: Notice,
            txn: &str,
        ) -> Result<String> {
            if *self.fail.lock().unwrap() {
                anyhow::bail!("matrix down");
            }
            let mut sent = self.sent.lock().unwrap();
            // like a homeserver: a resend with the same transaction ID is dropped
            let i = match sent.iter().position(|(_, _, t)| t == txn) {
                Some(i) => i,
                None => {
                    sent.push((room.to_string(), markdown.to_owned(), txn.to_owned()));
                    self.notices.lock().unwrap().push((txn.to_owned(), notice));
                    sent.len() - 1
                }
            };
            Ok(format!("$ev-{i}"))
        }
        async fn edit(&self, room: &RoomId, event: &str, markdown: &str, txn: &str) -> Result<()> {
            if *self.fail.lock().unwrap() {
                anyhow::bail!("matrix down");
            }
            let mut edits = self.edits.lock().unwrap();
            if !edits.iter().any(|(_, _, _, t)| t == txn) {
                edits.push((
                    room.to_string(),
                    event.to_owned(),
                    markdown.to_owned(),
                    txn.to_owned(),
                ));
            }
            Ok(())
        }
        async fn send_mentioning(
            &self,
            room: &RoomId,
            markdown: &str,
            users: &[OwnedUserId],
            notice: Notice,
            txn: &str,
        ) -> Result<()> {
            self.send(room, markdown, notice, txn).await?;
            if !users.is_empty() {
                self.pinged.lock().unwrap().push((
                    txn.to_owned(),
                    users.iter().map(|u| u.to_string()).collect(),
                ));
            }
            Ok(())
        }
        async fn send_file(
            &self,
            room: &RoomId,
            name: &str,
            bytes: Vec<u8>,
            notice: Notice,
            txn: &str,
        ) -> Result<()> {
            if *self.fail.lock().unwrap() || *self.fail_files.lock().unwrap() {
                anyhow::bail!("matrix down");
            }
            self.notices.lock().unwrap().push((txn.to_owned(), notice));
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
            if self
                .uninvitable
                .lock()
                .unwrap()
                .iter()
                .any(|u| u == user.as_str())
            {
                anyhow::bail!("M_FORBIDDEN");
            }
            self.members.lock().unwrap().push((
                room.to_string(),
                Member {
                    id: user.to_owned(),
                    admin: false,
                    joined: false,
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
        async fn tell_admins(&self, markdown: &str, key: &str) -> Result<()> {
            if *self.fail.lock().unwrap() {
                anyhow::bail!("matrix down");
            }
            let mut notes = self.admin_notes.lock().unwrap();
            // like a homeserver: a resend with the same transaction ID is dropped
            if !notes.iter().any(|(k, _)| k == key) {
                notes.push((key.to_owned(), markdown.to_owned()));
            }
            Ok(())
        }
        async fn message_person(&self, user: &UserId, markdown: &str, key: &str) -> Result<()> {
            if *self.fail.lock().unwrap()
                || self
                    .unknown
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|u| u == user.as_str())
            {
                anyhow::bail!("M_NOT_FOUND");
            }
            let mut dms = self.dms.lock().unwrap();
            if !dms.iter().any(|(_, _, k)| k == key) {
                dms.push((user.to_string(), markdown.to_owned(), key.to_owned()));
            }
            Ok(())
        }
        async fn resolve_alias(&self, alias: &str) -> Result<OwnedRoomId> {
            let aliases = self.aliases.lock().unwrap();
            let (_, room) = aliases
                .iter()
                .find(|(a, _)| a == alias)
                .context("M_NOT_FOUND")?;
            Ok(room.as_str().try_into()?)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fakes::*;
    use super::*;
    use chrono::{Datelike, TimeZone};
    use mxbot_common::directory::{Link, Person as DirPerson};

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
            require_group: None,
            access: Access::House,
            publication: Publication::Full,
            messages: crate::config::Messages::All,
            room_settings: true,
            page_after_days: None,
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
        "!house:x".try_into().unwrap()
    }

    fn cmd(s: &str) -> Command {
        crate::commands::parse(s).unwrap()
    }

    #[tokio::test]
    async fn a_meeting_from_preparation_to_summary() {
        let t = setup(vec![plenum("house", "House plenum", "!house:x")]);
        let e = &t.engine;

        // An item before the protocol exists waits for it.
        let a = e
            .command(&room(), &anna(), cmd("!top Müll"), at(2026, 10, 20, 12, 0))
            .await;
        assert!(a.contains("Noted"), "{a}");

        e.tick(at(2026, 10, 28, 18, 59)).await; // 4 days before: nothing yet
        assert!(e.wiki.by_title("House plenum 2026-11-01").is_none());

        e.tick(at(2026, 10, 29, 19, 0)).await; // 3 days before: prepared and announced
        let doc = e
            .wiki
            .by_title("House plenum 2026-11-01")
            .expect("protocol");
        assert!(
            doc.text
                .contains("### Müll\n\n**From:** Anna (from the chat)\n\n**Result:**"),
            "{}",
            doc.text
        );
        assert!(e.wiki.by_title("House plenum: Template").is_some());
        assert!(e.wiki.by_title("House plenum: Decisions").is_some());
        let texts = e.chat.texts();
        assert_eq!(texts.len(), 1);
        assert!(
            texts[0].contains("https://wiki.test/doc/house-plenum-2026-11-01"),
            "{}",
            texts[0]
        );
        assert_eq!(e.chat.kinds(), [Notice::Link]);

        // Ticks change nothing more; a second item goes straight into the document.
        e.tick(at(2026, 10, 29, 19, 1)).await;
        assert_eq!(e.chat.texts().len(), 1);
        let a = e
            .command(
                &room(),
                &anna(),
                cmd("!top Kaffee\nWe need a new machine."),
                at(2026, 10, 30, 9, 0),
            )
            .await;
        assert!(a.contains("On the page"), "{a}");
        let doc = e.wiki.by_title("House plenum 2026-11-01").unwrap();
        // above the marker, which stays below it for the next one
        let marker = page::TOPIC_MARKER;
        assert!(
            doc.text.contains(&format!(
                "### Kaffee\n\n**From:** Anna (from the chat)\n\nWe need a new machine.\n\n**Result:**\n\n{marker}"
            )),
            "{}",
            doc.text
        );
        let titles: Vec<String> = page::topics(&doc.text).iter().map(|t| t.title()).collect();
        assert_eq!(titles, ["Müll", "Kaffee"]);

        e.tick(at(2026, 11, 1, 18, 30)).await; // the reminder
        assert!(e.chat.texts()[1].contains("starts at 19:00"));

        // During the meeting: a result for Müll, Kaffee not reached, a task.
        {
            let mut docs = e.wiki.docs.lock().unwrap();
            let (d, _) = docs
                .iter_mut()
                .find(|(d, _)| d.title == "House plenum 2026-11-01")
                .unwrap();
            d.text = d.text.replacen(
                "**Result:**",
                "**Result:** talked\n\nDecision: Kompost kommt.",
                1,
            );
            d.text.push_str("\n- [ ] Ben: Tonne kaufen\n");
        }
        e.tick(at(2026, 11, 1, 21, 0)).await; // just ended: not closed yet
        assert_eq!(e.chat.texts().len(), 2);
        e.tick(at(2026, 11, 2, 9, 0)).await; // 12 h after the end: the result box goes in
        assert_eq!(e.chat.texts().len(), 2);
        let doc = e.wiki.by_title("House plenum 2026-11-01").unwrap();
        assert!(doc.text.contains("✅ Müll: Kompost kommt."), "{}", doc.text);
        assert!(
            doc.text
                .contains("- ⏭️ On the next page: Kaffee (not reached)"),
            "{}",
            doc.text
        );
        assert!(!doc.text.contains(page::RESULT_MARKER));
        e.tick(at(2026, 11, 2, 9, 1)).await; // seen there: closed
        let texts = e.chat.texts();
        assert_eq!(texts.len(), 3, "{texts:?}");
        assert!(texts[2].contains("- Kompost kommt."), "{}", texts[2]);
        assert!(texts[2].contains("- Ben: Tonne kaufen"));
        let decisions = e.wiki.by_title("House plenum: Decisions").unwrap();
        assert!(decisions.text.contains("### [House plenum 2026-11-01](https://wiki.test/doc/house-plenum-2026-11-01)\n\n* Kompost kommt."), "{}", decisions.text);
        if e.pdf.enabled {
            assert_eq!(e.wiki.attachments.lock().unwrap().len(), 1);
            assert_eq!(e.chat.files.lock().unwrap().len(), 1);
            let doc = e.wiki.by_title("House plenum 2026-11-01").unwrap();
            assert!(doc.text.contains(
                "The minutes as a PDF:\n\n[House plenum 2026-11-01.pdf](/api/attachments.redirect?id=att-1)"
            ));
        }

        // Done is done.
        e.tick(at(2026, 11, 2, 10, 0)).await;
        assert_eq!(e.chat.texts().len(), 3);
        assert_eq!(
            decisions.text,
            e.wiki.by_title("House plenum: Decisions").unwrap().text
        );
        // The topic not reached is on the next page once it opens, with where it comes from.
        e.tick(at(2026, 12, 3, 19, 0)).await;
        let next = e
            .wiki
            .by_title("House plenum 2026-12-06")
            .expect("next page");
        assert!(
            next.text
                .contains("### Kaffee\n\n*Carried over from the [plenum on "),
            "{}",
            next.text
        );
        assert!(next.text.contains("We need a new machine."));
        assert!(!next.text.contains("### Müll"));
        assert!(next.text.contains("- [ ] Ben: Tonne kaufen"));
        e.tick(at(2026, 12, 3, 19, 1)).await; // seen there: no longer waiting
        assert!(e.state.lock().await.plenums["house"].open_topics.is_empty());
        let next2 = e.wiki.by_title("House plenum 2026-12-06").unwrap();
        assert_eq!(next2.text.matches("### Kaffee").count(), 1);
    }

    #[tokio::test]
    async fn a_failed_step_is_repeated_and_the_done_ones_are_not() {
        let t = setup(vec![plenum("house", "House plenum", "!house:x")]);
        let e = &t.engine;
        *e.wiki.fail.lock().unwrap() = true;
        e.tick(at(2026, 10, 29, 19, 0)).await;
        assert!(e.wiki.docs.lock().unwrap().is_empty());
        *e.wiki.fail.lock().unwrap() = false;
        *e.chat.fail.lock().unwrap() = true; // created, but the announcement fails
        e.tick(at(2026, 10, 29, 19, 1)).await;
        assert!(e.wiki.by_title("House plenum 2026-11-01").is_some());
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
            .filter(|(d, _)| d.title.starts_with("House plenum 2026"))
            .count();
        assert_eq!(protocols, 1, "no second protocol");
    }

    #[tokio::test]
    async fn a_restart_finds_the_protocol_it_created_before_the_crash() {
        let t = setup(vec![plenum("house", "House plenum", "!house:x")]);
        let e = &t.engine;
        e.tick(at(2026, 10, 29, 19, 0)).await;
        // As if the bot had crashed after creating the document but before saving it.
        {
            let mut st = e.state.lock().await;
            let m = st
                .plenums
                .get_mut("house")
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
            .filter(|(d, _)| d.title.starts_with("House plenum 2026"))
            .count();
        assert_eq!(protocols, 1);
    }

    #[tokio::test]
    async fn the_bot_closes_early_on_request_and_skips_what_it_missed() {
        let t = setup(vec![plenum("house", "House plenum", "!house:x")]);
        let e = &t.engine;
        e.tick(at(2026, 10, 29, 19, 0)).await;
        let a = e
            .command(
                &room(),
                &anna(),
                cmd("!minutes done"),
                at(2026, 10, 30, 12, 0),
            )
            .await;
        assert!(a.contains("no open minutes"), "not started yet: {a}");
        // while it is on, only an organizer or an admin closes it (a close is final)
        let a = e
            .command(
                &room(),
                &anna(),
                cmd("!minutes done"),
                at(2026, 11, 1, 20, 0),
            )
            .await;
        assert!(a.contains("on until 21:00"), "{a}");
        let admin = Sender {
            id: "@admin:x".try_into().unwrap(),
            name: "Admin".into(),
        };
        let a = e
            .command(
                &room(),
                &admin,
                cmd("!minutes done"),
                at(2026, 11, 1, 20, 0),
            )
            .await;
        assert!(a.contains("is closed in a moment"), "{a}");
        e.tick(at(2026, 11, 1, 20, 1)).await;
        e.tick(at(2026, 11, 1, 20, 2)).await;
        assert!(e.chat.texts().last().unwrap().contains("are closed:"));

        // The December meeting passes while the bot is down: skipped, not prepared late.
        e.tick(at(2026, 12, 7, 12, 0)).await;
        assert!(e.wiki.by_title("House plenum 2026-12-06").is_none());
        let st = e.state.lock().await;
        assert!(st.plenums["house"].meetings["house-series#2026-12-06"].skipped);
    }

    #[tokio::test]
    async fn the_calendar_cancels_adds_and_brings_back_dates() {
        let t = setup(vec![plenum("house", "House plenum", "!house:x")]);
        let e = &t.engine;
        let now = at(2026, 10, 20, 12, 0);
        // cancelled before it was prepared: no protocol, nothing said
        calendar(
            e,
            "house",
            |l| l.retain(|o| o.id != "house-series#2026-11-01"),
            now,
        )
        .await;
        // an extra date
        calendar(
            e,
            "house",
            |l| l.push(occ("extra-1", "2026-11-04 20:00")),
            now,
        )
        .await;
        let a = e.command(&room(), &anna(), cmd("!plenum"), now).await;
        assert!(
            a.contains("Next meeting: Wednesday, 4 November 2026 at 20:00"),
            "{a}"
        );
        assert!(a.contains("calendar “House plenum”"), "{a}");
        e.tick(at(2026, 10, 29, 19, 0)).await;
        assert!(e.wiki.by_title("House plenum 2026-11-01").is_none());
        e.tick(at(2026, 11, 1, 20, 0)).await;
        assert!(e.wiki.by_title("House plenum 2026-11-04").is_some());
        assert_eq!(e.chat.texts().len(), 1, "announced once");

        // announced, then cancelled: said once, the protocol stays
        e.tick(at(2026, 12, 3, 19, 0)).await; // 6.12. announced (and the 4.11. closing)
        e.tick(at(2026, 12, 3, 19, 1)).await;
        let before = e.chat.texts().len();
        assert!(e.chat.texts().iter().any(|t| t.contains("6 December 2026")));
        calendar(
            e,
            "house",
            |l| l.retain(|o| o.id != "house-series#2026-12-06"),
            at(2026, 12, 4, 9, 0),
        )
        .await;
        e.tick(at(2026, 12, 4, 9, 0)).await;
        e.tick(at(2026, 12, 4, 9, 1)).await;
        let texts = e.chat.texts();
        assert_eq!(texts.len(), before + 1, "{texts:?}");
        let last = texts.last().unwrap();
        assert!(
            last.contains("cancelled") && last.contains("6 December 2026"),
            "{last}"
        );
        // the page stays, its title says it
        assert!(e
            .wiki
            .by_title("House plenum 2026-12-06 (cancelled)")
            .is_some());
        e.tick(at(2026, 12, 6, 18, 30)).await; // no reminder for it
        assert_eq!(e.chat.texts().len(), before + 1);

        // the old commands point to the calendar
        let a = e
            .command(&room(), &anna(), cmd("!plenum cancel 3.1."), now)
            .await;
        assert!(a.contains("Nextcloud calendar"), "{a}");
    }

    #[tokio::test]
    async fn a_cancelled_meeting_that_comes_back_goes_on() {
        let t = setup(vec![plenum("house", "House plenum", "!house:x")]);
        let e = &t.engine;
        e.tick(at(2026, 10, 29, 19, 0)).await; // 1.11. announced
        let gone = e.state.lock().await.plenums["house"].occurrences.clone();
        calendar(
            e,
            "house",
            |l| l.retain(|o| o.id != "house-series#2026-11-01"),
            at(2026, 10, 30, 9, 0),
        )
        .await;
        e.tick(at(2026, 10, 30, 9, 0)).await;
        assert!(e.chat.texts().last().unwrap().contains("cancelled"));
        // undone in the calendar: the meeting goes on with its protocol
        e.apply_calendar("house", gone, at(2026, 10, 30, 10, 0))
            .await;
        // the room was told it is cancelled: it hears at once that it is on again
        e.tick(at(2026, 10, 30, 10, 0)).await;
        let texts = e.chat.texts();
        assert_eq!(texts.len(), 3, "{texts:?}");
        assert!(
            texts[2].starts_with("📝 **House plenum** on Sunday, 1 November"),
            "{}",
            texts[2]
        );
        e.tick(at(2026, 11, 1, 18, 30)).await;
        assert!(e.chat.texts().last().unwrap().contains("starts at 19:00"));
    }

    #[tokio::test]
    async fn the_calendar_unread_changes_nothing() {
        let t = setup(vec![plenum("house", "House plenum", "!house:x")]);
        let e = &t.engine;
        e.tick(at(2026, 10, 29, 19, 0)).await;
        // a calendar that was never read knows nothing: nothing is cancelled
        {
            let mut st = e.state.lock().await;
            let ps = st.plenums.get_mut("house").unwrap();
            ps.calendar_read = None;
            ps.occurrences.clear();
        }
        e.tick(at(2026, 10, 30, 9, 0)).await;
        assert_eq!(e.chat.texts().len(), 1);
        assert!(
            !e.state.lock().await.plenums["house"].meetings["house-series#2026-11-01"].cancelled
        );
    }

    #[tokio::test]
    async fn several_plenums_in_one_room_need_the_key() {
        let t = setup(vec![
            plenum(
                "kitchen-floor-0",
                "Kitchen plenum ground floor",
                "!kitchens:x",
            ),
            plenum("kitchen-floor-3", "Kitchen plenum 3rd floor", "!kitchens:x"),
        ]);
        let e = &t.engine;
        let r: OwnedRoomId = "!kitchens:x".try_into().unwrap();
        let now = at(2026, 10, 20, 12, 0);
        let a = e.command(&r, &anna(), cmd("!top Spülmaschine"), now).await;
        assert!(
            a.contains("several plenums") && a.contains("`kitchen-floor-0`"),
            "{a}"
        );
        let a = e
            .command(
                &r,
                &anna(),
                crate::commands::parse_for(
                    "!top kitchen-floor-3 Spülmaschine",
                    &["kitchen-floor-0", "kitchen-floor-3"],
                )
                .unwrap(),
                now,
            )
            .await;
        assert!(a.contains("Noted for Kitchen plenum 3rd floor"), "{a}");
        let a = e.command(&room(), &anna(), cmd("!top x"), now).await;
        assert!(a.contains("no plenum"), "{a}");
        // Both get their own template and decision list in the shared collection.
        e.tick(at(2026, 10, 29, 19, 0)).await;
        assert!(e
            .wiki
            .by_title("Kitchen plenum ground floor: Template")
            .is_some());
        assert!(e
            .wiki
            .by_title("Kitchen plenum 3rd floor: Decisions")
            .is_some());
        let og = e
            .wiki
            .by_title("Kitchen plenum 3rd floor 2026-11-01")
            .unwrap();
        assert!(og.text.contains("Spülmaschine"));
        let eg = e
            .wiki
            .by_title("Kitchen plenum ground floor 2026-11-01")
            .unwrap();
        assert!(!eg.text.contains("Spülmaschine"));
    }

    #[tokio::test]
    async fn a_template_edited_in_the_wiki_is_used() {
        let t = setup(vec![plenum("house", "House plenum", "!house:x")]);
        let e = &t.engine;
        e.tick(at(2026, 10, 29, 19, 0)).await; // creates the template (and November's protocol)
        {
            let mut docs = e.wiki.docs.lock().unwrap();
            let (d, _) = docs
                .iter_mut()
                .find(|(d, _)| d.title == "House plenum: Template")
                .unwrap();
            d.text = "Our own template for {{plenum}} on {{iso_date}}\n\n## Agenda\n".into();
            d.untouched = false; // an edit in Outline sets updatedAt
        }
        e.tick(at(2026, 12, 3, 19, 0)).await;
        let dec = e.wiki.by_title("House plenum 2026-12-06").unwrap();
        assert!(
            dec.text
                .starts_with("Our own template for House plenum on 2026-12-06"),
            "{}",
            dec.text
        );
    }

    /// A document as the old bot left it: its text in Outline's own Markdown, changed since it
    /// was made (by the bot's own renewal).
    fn as_the_old_bot_left_it(e: &Engine<FakeWiki, FakeChat>, title: &str, text: &str) {
        let mut docs = e.wiki.docs.lock().unwrap();
        let (d, _) = docs
            .iter_mut()
            .find(|(d, del)| d.title == title && !del)
            .unwrap();
        d.text = text.replace("[]", "\\[\\]").replace("\n\n", "\n\n\n");
        d.untouched = false;
    }

    #[tokio::test]
    async fn the_bots_former_template_and_example_get_the_new_texts() {
        let t = setup(vec![plenum("house", "House plenum", "!house:x")]);
        let e = &t.engine;
        e.tick(at(2026, 10, 1, 12, 0)).await; // the area, no page yet
        let p = protocol::tests_plenum();
        as_the_old_bot_left_it(e, "House plenum: Template", &former::templates()[1]);
        as_the_old_bot_left_it(e, "House plenum: Example minutes", &former::examples(&p)[0]);
        // state.json of the old bot: no fingerprints
        e.state
            .lock()
            .await
            .plenums
            .get_mut("house")
            .unwrap()
            .written
            .clear();
        e.tick(at(2026, 10, 1, 13, 1)).await; // the area checked again
        let tpl = e.wiki.by_title("House plenum: Template").unwrap();
        assert_eq!(tpl.text, protocol::DEFAULT_TEMPLATE);
        let eg = e.wiki.by_title("House plenum: Example minutes").unwrap();
        assert_eq!(eg.text, protocol::example_doc_text(&p));
        e.tick(at(2026, 10, 29, 19, 0)).await;
        let nov = e.wiki.by_title("House plenum 2026-11-01").unwrap();
        assert!(nov.text.contains("## Topics"), "{}", nov.text);
    }

    #[tokio::test]
    async fn the_text_the_bot_wrote_last_is_renewed_too() {
        let t = setup(vec![plenum("house", "House plenum", "!house:x")]);
        let e = &t.engine;
        e.tick(at(2026, 10, 1, 12, 0)).await;
        // the bot renewed it once (no longer untouched) with a text that has changed since
        let id = e.wiki.by_title("House plenum: Rules").unwrap().id;
        as_the_old_bot_left_it(
            e,
            "House plenum: Rules",
            "What applies: the bot's text of a version to come",
        );
        e.state
            .lock()
            .await
            .plenums
            .get_mut("house")
            .unwrap()
            .written
            .insert(
                id,
                former::fingerprint("What applies: the bot's text of a version to come"),
            );
        e.tick(at(2026, 10, 1, 13, 1)).await;
        let rules = e.wiki.by_title("House plenum: Rules").unwrap();
        assert_eq!(
            rules.text,
            protocol::rules_doc_text(&protocol::tests_plenum())
        );
        // and a person's change after that stays
        edit_doc(e, "House plenum: Rules", |t| {
            t.push_str("\nQuiet after 22:00.\n")
        });
        let writes = *e.wiki.writes.lock().unwrap();
        e.tick(at(2026, 10, 1, 14, 2)).await;
        assert!(e
            .wiki
            .by_title("House plenum: Rules")
            .unwrap()
            .text
            .contains("Quiet after 22:00."));
        assert_eq!(*e.wiki.writes.lock().unwrap(), writes, "nothing written");
    }

    #[tokio::test]
    async fn a_renewal_the_wiki_refuses_does_not_stop_the_page() {
        let t = setup(vec![plenum("house", "House plenum", "!house:x")]);
        let e = &t.engine;
        e.tick(at(2026, 10, 1, 12, 0)).await;
        as_the_old_bot_left_it(e, "House plenum: Template", &former::templates()[1]);
        *e.wiki.no_replace.lock().unwrap() = true; // archived in the wiki: Outline 403
        e.tick(at(2026, 10, 29, 19, 0)).await;
        let nov = e.wiki.by_title("House plenum 2026-11-01").unwrap();
        assert!(
            nov.text.contains("## Agenda"),
            "the old template: {}",
            nov.text
        );
        *e.wiki.no_replace.lock().unwrap() = false;
        e.tick(at(2026, 10, 29, 20, 1)).await;
        let tpl = e.wiki.by_title("House plenum: Template").unwrap();
        assert_eq!(tpl.text, protocol::DEFAULT_TEMPLATE, "tried again");
    }

    fn edit_doc(e: &Engine<FakeWiki, FakeChat>, title: &str, f: impl FnOnce(&mut String)) {
        let mut docs = e.wiki.docs.lock().unwrap();
        let (d, _) = docs
            .iter_mut()
            .find(|(d, del)| d.title == title && !del)
            .unwrap();
        f(&mut d.text);
        d.untouched = false; // an edit in Outline sets updatedAt
    }

    fn delete_doc(e: &Engine<FakeWiki, FakeChat>, title: &str) {
        let mut docs = e.wiki.docs.lock().unwrap();
        docs.iter_mut().find(|(d, _)| d.title == title).unwrap().1 = true;
    }

    #[tokio::test]
    async fn a_chat_item_changes_nothing_else_and_decides_nothing() {
        let t = setup(vec![plenum("house", "House plenum", "!house:x")]);
        let e = &t.engine;
        e.tick(at(2026, 10, 29, 19, 0)).await;
        edit_doc(e, "House plenum 2026-11-01", |t| {
            t.push_str("\n* Variante [x] Hof oder [ ] Keller\n")
        });
        let a = e
            .command(
                &room(),
                &anna(),
                cmd("!top Decision: Kasse an Anna [ ] sofort"),
                at(2026, 10, 30, 9, 0),
            )
            .await;
        assert!(a.contains("On the page"), "{a}");
        let doc = e.wiki.by_title("House plenum 2026-11-01").unwrap();
        assert!(
            doc.text.contains("* Variante [x] Hof oder [ ] Keller"),
            "untouched: {}",
            doc.text
        );
        assert!(
            doc.text
                .contains("### Decision: Kasse an Anna \\[ \\] sofort\n\n**From:** Anna"),
            "{}",
            doc.text
        );
        let a = e
            .command(
                &room(),
                &anna(),
                cmd("!minutes done"),
                at(2026, 11, 1, 21, 5),
            )
            .await;
        assert!(a.contains("is closed in a moment"), "{a}");
        e.tick(at(2026, 11, 1, 21, 6)).await;
        e.tick(at(2026, 11, 1, 21, 7)).await;
        let summary = e.chat.texts().last().unwrap().clone();
        assert!(summary.contains("No decisions"), "{summary}");
        assert!(!summary.contains("Kasse"), "{summary}");
    }

    #[tokio::test]
    async fn a_page_of_the_old_template_gets_agenda_lines() {
        let t = setup(vec![plenum("house", "House plenum", "!house:x")]);
        let e = &t.engine;
        e.tick(at(2026, 10, 29, 19, 0)).await;
        // as the pages made before 2026-11-07 are
        edit_doc(e, "House plenum 2026-11-01", |t| {
            *t = format!(
                "## Agenda\n\n* Hof (Ben)\n\n{}\n\n## Minutes\n",
                protocol::TOP_MARKER
            )
        });
        let a = e
            .command(
                &room(),
                &anna(),
                cmd("!top Decision: Kasse an Anna [ ] sofort"),
                at(2026, 10, 30, 9, 0),
            )
            .await;
        assert!(a.contains("On the page"), "{a}");
        let doc = e.wiki.by_title("House plenum 2026-11-01").unwrap();
        assert!(
            doc.text.contains(&format!(
                "* Item: Decision: Kasse an Anna ( ) sofort (Anna)\n\n{}",
                protocol::TOP_MARKER
            )),
            "{}",
            doc.text
        );
        assert!(protocol::extract(&doc.text).decisions.is_empty());
    }

    #[tokio::test]
    async fn without_the_marker_items_go_under_a_heading_of_their_own() {
        let t = setup(vec![plenum("house", "House plenum", "!house:x")]);
        let e = &t.engine;
        e.tick(at(2026, 10, 29, 19, 0)).await;
        edit_doc(e, "House plenum 2026-11-01", |t| {
            *t = t.replace(page::TOPIC_MARKER, "")
        });
        for item in ["!top Garten", "!top Keller"] {
            e.command(&room(), &anna(), cmd(item), at(2026, 10, 30, 9, 0))
                .await;
        }
        // without the marker: at the end of the topics, so they are read as topics
        let doc = e.wiki.by_title("House plenum 2026-11-01").unwrap();
        let titles: Vec<String> = page::topics(&doc.text).iter().map(|t| t.title()).collect();
        assert_eq!(titles, ["Garten", "Keller"], "{}", doc.text);
        // and on a page without topics at all, under a heading of their own at the end
        edit_doc(e, "House plenum 2026-11-01", |t| {
            *t = "Notes only.\n".to_owned()
        });
        e.command(&room(), &anna(), cmd("!top Dach"), at(2026, 10, 30, 9, 0))
            .await;
        let doc = e.wiki.by_title("House plenum 2026-11-01").unwrap();
        assert!(
            doc.text
                .ends_with("## New items from the chat\n\n* Dach (Anna)"),
            "{}",
            doc.text
        );
        edit_doc(e, "House plenum 2026-11-01", |t| {
            *t = protocol::render_template(
                protocol::DEFAULT_TEMPLATE,
                &e.plenums[0],
                "2026-11-01 19:00".parse().unwrap(),
                &[],
            )
        });
        e.command(&room(), &anna(), cmd("!top Garten"), at(2026, 10, 30, 9, 0))
            .await;
        // Sent again (a lost answer, a second try): not twice.
        e.command(&room(), &anna(), cmd("!top Garten"), at(2026, 10, 30, 9, 1))
            .await;
        let doc = e.wiki.by_title("House plenum 2026-11-01").unwrap();
        assert_eq!(doc.text.matches("Garten").count(), 1);
    }

    #[tokio::test]
    async fn a_deleted_decision_list_or_collection_comes_back() {
        let t = setup(vec![plenum("house", "House plenum", "!house:x")]);
        let e = &t.engine;
        e.tick(at(2026, 10, 29, 19, 0)).await;
        delete_doc(e, "House plenum: Decisions");
        edit_doc(e, "House plenum 2026-11-01", |t| {
            t.push_str("\nDecision: Kompost.\n")
        });
        e.tick(at(2026, 11, 2, 9, 0)).await;
        e.tick(at(2026, 11, 2, 9, 1)).await;
        let list = e
            .wiki
            .by_title("House plenum: Decisions")
            .expect("created again");
        assert!(list.text.contains("Kompost."), "{}", list.text);
        assert!(e.chat.texts().last().unwrap().contains("are closed:"));
        // The collection is looked up again for the next protocol.
        e.wiki.collections.lock().unwrap().clear();
        e.tick(at(2026, 12, 3, 19, 0)).await;
        assert!(e.wiki.by_title("House plenum 2026-12-06").is_some());
    }

    #[tokio::test]
    async fn one_failing_meeting_does_not_hold_up_the_next() {
        let mut p = plenum("house", "House plenum", "!house:x");
        p.prepare_days = 8;
        let t = setup(vec![p]);
        let e = &t.engine;
        calendar(
            e,
            "house",
            |l| l.push(occ("weekly#2026-11-08", "2026-11-08 19:00")),
            at(2026, 10, 29, 0, 0),
        )
        .await;
        e.tick(at(2026, 10, 29, 19, 0)).await; // Nov 1 and Nov 8 prepared
        delete_doc(e, "House plenum 2026-11-01");
        // Nov 1 closes without a protocol; Nov 8 is unaffected.
        e.tick(at(2026, 11, 2, 9, 0)).await;
        assert!(e.wiki.by_title("House plenum 2026-11-08").is_some());
        let st = e.state.lock().await;
        assert!(
            st.plenums["house"].meetings["house-series#2026-11-01"]
                .close
                .done
        );
    }

    #[tokio::test]
    async fn a_pdf_that_does_not_reach_the_room_is_still_in_the_wiki() {
        let t = setup(vec![plenum("house", "House plenum", "!house:x")]);
        let e = &t.engine;
        if !e.pdf.enabled {
            return;
        }
        e.tick(at(2026, 10, 29, 19, 0)).await;
        *e.chat.fail_files.lock().unwrap() = true;
        // one tick for the result box, then the PDF's attempts
        for minute in 0..=PDF_ATTEMPTS {
            e.tick(at(2026, 11, 2, 9, minute)).await;
        }
        let summary = e.chat.texts().last().unwrap().clone();
        assert!(summary.contains("are closed:"), "{summary}");
        assert!(
            summary.contains("attached to the minutes in the wiki"),
            "{summary}"
        );
        assert!(!summary.contains("could not be made"), "{summary}");
        assert_eq!(e.wiki.attachments.lock().unwrap().len(), 1, "attached once");
    }

    #[tokio::test]
    async fn two_meetings_on_one_day_get_two_protocols() {
        let t = setup(vec![plenum("house", "House plenum", "!house:x")]);
        let e = &t.engine;
        calendar(
            e,
            "house",
            |l| l.push(occ("noon", "2026-11-01 12:00")),
            at(2026, 10, 28, 0, 0),
        )
        .await;
        e.tick(at(2026, 10, 29, 19, 0)).await;
        // the evening's page opens a day before, though the noon meeting's is still open
        assert!(e.wiki.by_title("House plenum 2026-11-01 19:00").is_none());
        e.tick(at(2026, 10, 31, 19, 0)).await;
        let titles: Vec<String> = e
            .wiki
            .docs
            .lock()
            .unwrap()
            .iter()
            .map(|(d, _)| d.title.clone())
            .filter(|t| t.starts_with("House plenum 2026-11-01"))
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
    async fn two_meetings_on_one_day_keep_their_decisions_and_the_later_ones_tasks() {
        let t = setup(vec![plenum("house", "House plenum", "!house:x")]);
        let e = &t.engine;
        calendar(
            e,
            "house",
            |l| l.push(occ("noon", "2026-11-01 12:00")),
            at(2026, 10, 28, 0, 0),
        )
        .await;
        e.tick(at(2026, 10, 29, 19, 0)).await;
        e.tick(at(2026, 10, 31, 19, 0)).await; // the evening's page a day before
        edit_doc(e, "House plenum 2026-11-01", |t| {
            t.push_str("\nDecision: noon decides.\n- [ ] Ann: noon task\n")
        });
        edit_doc(e, "House plenum 2026-11-01 19:00", |t| {
            t.push_str("\nDecision: evening decides.\n- [ ] Ben: evening task\n")
        });
        // the evening closes first (asked for), the noon one later (by the time)
        let admin = Sender {
            id: "@admin:x".try_into().unwrap(),
            name: "Admin".into(),
        };
        e.command(
            &room(),
            &admin,
            cmd("!minutes done"),
            at(2026, 11, 1, 19, 30),
        )
        .await;
        e.tick(at(2026, 11, 1, 19, 31)).await;
        e.tick(at(2026, 11, 1, 19, 32)).await;
        e.tick(at(2026, 11, 2, 9, 0)).await;
        e.tick(at(2026, 11, 2, 9, 1)).await;
        let st = e.state.lock().await;
        let ps = &st.plenums["house"];
        let decisions: Vec<&str> = ps.decisions.iter().map(|d| d.text.as_str()).collect();
        assert_eq!(decisions.len(), 2, "{decisions:?}");
        assert!(decisions.contains(&"noon decides.") && decisions.contains(&"evening decides."));
        // the evening's tasks go on: the noon meeting was earlier
        let tasks: Vec<&str> = ps.open_tasks.iter().map(|d| d.text.as_str()).collect();
        assert_eq!(tasks, ["Ben: evening task"]);
    }

    #[tokio::test]
    async fn open_tasks_reach_minutes_made_already() {
        let t = setup(vec![plenum("house", "House plenum", "!house:x")]);
        let e = &t.engine;
        // the next meeting two days later: prepared before this one closes
        calendar(
            e,
            "house",
            |l| l.push(occ("soon", "2026-11-03 19:00")),
            at(2026, 10, 28, 0, 0),
        )
        .await;
        e.tick(at(2026, 10, 31, 19, 0)).await;
        edit_doc(e, "House plenum 2026-11-01", |t| {
            t.push_str("\n- [ ] Ben: buy a bin [big]\n")
        });
        e.tick(at(2026, 11, 2, 9, 0)).await; // the 1st closed, the 3rd prepared before
        let next = e.wiki.by_title("House plenum 2026-11-03").unwrap();
        assert!(
            next.text.contains(protocol::OPEN_TASKS_HEADING)
                && next
                    .text
                    .contains("- [ ] Ben: buy a bin (big) (since 1 Nov 2026)"),
            "{}",
            next.text
        );
        // once
        e.tick(at(2026, 11, 2, 9, 1)).await;
        let again = e.wiki.by_title("House plenum 2026-11-03").unwrap();
        assert_eq!(again.text.matches("Ben: buy a bin").count(), 1);
    }

    #[tokio::test]
    async fn a_failed_save_stops_the_step() {
        let mut t = setup(vec![plenum("house", "House plenum", "!house:x")]);
        // the state file cannot be written (its place is a directory)
        let blocked = t._dir.path().join("blocked");
        std::fs::create_dir_all(blocked.join("x")).unwrap();
        t.engine.state_path = blocked;
        let e = &t.engine;
        e.tick(at(2026, 10, 29, 19, 0)).await;
        assert!(
            e.chat.texts().is_empty(),
            "nothing is sent that is not on disk"
        );
        assert!(e.wiki.by_title("House plenum 2026-11-01").is_none());
    }

    #[tokio::test]
    async fn a_private_collection_is_never_taken_over_by_its_name() {
        let t = setup(vec![finance()]);
        let e = &t.engine;
        e.wiki
            .collections
            .lock()
            .unwrap()
            .push(("col-other".into(), "Finance group".into()));
        e.tick(at(2026, 10, 20, 12, 0)).await;
        let id = e.state.lock().await.plenums["finance"]
            .collection_id
            .clone();
        assert!(id.is_some_and(|id| id != "col-other"));
    }

    #[tokio::test]
    async fn a_move_is_not_lost_when_the_wiki_or_the_room_fails_meanwhile() {
        let t = setup(vec![plenum("house", "House plenum", "!house:x")]);
        let e = &t.engine;
        e.tick(at(2026, 10, 29, 19, 0)).await; // announced
        let move_to = |l: &mut Vec<Occurrence>| {
            for o in l.iter_mut().filter(|o| o.id == "house-series#2026-11-01") {
                *o = occ(&o.id, "2026-11-08 18:00");
            }
        };
        calendar(e, "house", move_to, at(2026, 10, 30, 9, 0)).await;
        // Outline and the room are down at the tick that sees the move
        *e.wiki.fail.lock().unwrap() = true;
        *e.chat.fail.lock().unwrap() = true;
        e.tick(at(2026, 10, 30, 9, 0)).await;
        assert_eq!(e.chat.texts().len(), 1);
        // back: renamed and told, from the date the room knew
        *e.wiki.fail.lock().unwrap() = false;
        *e.chat.fail.lock().unwrap() = false;
        e.tick(at(2026, 10, 30, 9, 1)).await;
        assert!(e.wiki.by_title("House plenum 2026-11-08").is_some());
        let texts = e.chat.texts();
        assert_eq!(texts.len(), 2, "{texts:?}");
        assert!(
            texts[1].contains("moved from Sunday, 1 November 2026"),
            "{}",
            texts[1]
        );
    }

    #[tokio::test]
    async fn a_cancellation_is_told_once_the_room_answers_again() {
        let t = setup(vec![plenum("house", "House plenum", "!house:x")]);
        let e = &t.engine;
        e.tick(at(2026, 10, 29, 19, 0)).await; // announced
        calendar(
            e,
            "house",
            |l| l.retain(|o| o.id != "house-series#2026-11-01"),
            at(2026, 10, 30, 9, 0),
        )
        .await;
        *e.chat.fail.lock().unwrap() = true;
        e.tick(at(2026, 10, 30, 9, 0)).await;
        *e.chat.fail.lock().unwrap() = false;
        e.tick(at(2026, 10, 30, 9, 1)).await;
        e.tick(at(2026, 10, 30, 9, 2)).await;
        let texts = e.chat.texts();
        assert_eq!(texts.len(), 2, "{texts:?}");
        assert!(texts[1].contains("cancelled"), "{}", texts[1]);
    }

    #[tokio::test]
    async fn a_calendar_made_anew_cancels_nothing_until_it_has_events() {
        let t = setup(vec![plenum("house", "House plenum", "!house:x")]);
        let e = &t.engine;
        e.tick(at(2026, 10, 29, 19, 0)).await; // announced
        e.calendar_remade("house").await;
        e.apply_calendar("house", vec![], at(2026, 10, 30, 9, 0))
            .await;
        e.tick(at(2026, 10, 30, 9, 0)).await;
        assert_eq!(e.chat.texts().len(), 1, "nothing cancelled");
        assert_eq!(e.state.lock().await.plenums["house"].occurrences.len(), 9);
        // once it has events again, they count (and the guard is gone)
        e.apply_calendar(
            "house",
            vec![occ("new", "2026-11-04 20:00")],
            at(2026, 10, 30, 10, 0),
        )
        .await;
        let st = e.state.lock().await;
        assert!(!st.plenums["house"].calendar_remade);
        assert_eq!(st.plenums["house"].occurrences.len(), 1);
    }

    #[tokio::test]
    async fn a_meeting_back_after_its_start_is_not_announced_again() {
        let t = setup(vec![plenum("house", "House plenum", "!house:x")]);
        let e = &t.engine;
        e.tick(at(2026, 10, 29, 19, 0)).await; // announced
        let all = e.state.lock().await.plenums["house"].occurrences.clone();
        calendar(
            e,
            "house",
            |l| l.retain(|o| o.id != "house-series#2026-11-01"),
            at(2026, 10, 30, 9, 0),
        )
        .await;
        e.tick(at(2026, 10, 30, 9, 0)).await; // cancelled, said
        e.apply_calendar("house", all, at(2026, 11, 1, 19, 30))
            .await;
        e.tick(at(2026, 11, 1, 19, 30)).await;
        let texts = e.chat.texts();
        assert!(
            !texts.iter().skip(2).any(|t| t.starts_with("📋")),
            "no announcement after the start: {texts:?}"
        );
    }

    #[tokio::test]
    async fn organizers_close_early_and_others_may_not() {
        let mut p = plenum("house", "House plenum", "!house:x");
        p.organizers = HashSet::from(["@ben:x".try_into().unwrap()]);
        let t = setup(vec![p]);
        let e = &t.engine;
        e.tick(at(2026, 10, 29, 19, 0)).await;
        let a = e
            .command(
                &room(),
                &anna(),
                cmd("!minutes done"),
                at(2026, 11, 1, 20, 0),
            )
            .await;
        assert!(a.starts_with("⛔"), "{a}");
        let ben = Sender {
            id: "@ben:x".try_into().unwrap(),
            name: "Ben".into(),
        };
        let a = e
            .command(&room(), &ben, cmd("!minutes done"), at(2026, 11, 1, 20, 0))
            .await;
        assert!(a.contains("is closed in a moment"), "{a}");
    }

    #[tokio::test]
    async fn a_reminder_after_the_start_says_so() {
        let t = setup(vec![plenum("house", "House plenum", "!house:x")]);
        let e = &t.engine;
        e.tick(at(2026, 11, 1, 19, 30)).await; // the bot comes back during the meeting
        let texts = e.chat.texts();
        assert!(
            texts.last().unwrap().contains("started at 19:00"),
            "{texts:?}"
        );
    }

    fn kitchen() -> Plenum {
        Plenum {
            key: "kitchen-floor-1".into(),
            name: "Kitchen plenum 1st floor".into(),
            room: "!k1:x".try_into().unwrap(),
            collection: "Kitchen 1st floor".into(),
            group: Some("kitchen-floor-1".into()),
            access: Access::Members,
            ..plenum("kitchen-floor-1", "Kitchen plenum 1st floor", "!k1:x")
        }
    }

    fn finance() -> Plenum {
        Plenum {
            collection: "Finance group".into(),
            group: Some("finance".into()),
            access: Access::Private,
            ..plenum("finance", "Finance plenum", "!fin:x")
        }
    }

    #[tokio::test]
    async fn every_plenum_gets_its_area_with_the_access_its_group_needs() {
        let t = setup(vec![
            plenum("house", "House plenum", "!house:x"),
            kitchen(),
            finance(),
        ]);
        let e = &t.engine;
        e.wiki.groups.lock().unwrap().push("kitchen-floor-1".into()); // finance: not yet in the wiki
        e.tick(at(2026, 10, 20, 12, 0)).await;
        for title in [
            "Kitchen plenum 1st floor: Overview",
            "Kitchen plenum 1st floor: Rules",
            "Kitchen plenum 1st floor: Minutes",
            "Kitchen plenum 1st floor: Decisions",
            "Kitchen plenum 1st floor: Template",
            "House plenum: Overview",
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
        assert_eq!(of("Kitchen 1st floor"), Access::Members);
        assert_eq!(of("Finance group"), Access::Private);
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
        let o = e.wiki.by_title("House plenum: Overview").unwrap();
        assert!(
            o.text.contains("Sunday, 1 November 2026 at 19:00"),
            "{}",
            o.text
        );
        assert!(
            o.text
                .contains("[House plenum: Rules](https://wiki.test/doc/house-plenum:-rules)"),
            "{}",
            o.text
        );
        // written only when it changes
        let writes = *e.wiki.writes.lock().unwrap();
        e.tick(at(2026, 10, 20, 13, 2)).await;
        assert_eq!(*e.wiki.writes.lock().unwrap(), writes);
    }

    #[tokio::test]
    async fn an_area_from_before_is_renamed_not_made_again() {
        // The plenum was renamed in the config: another collection name, other titles.
        let t = setup(vec![kitchen()]);
        let e = &t.engine;
        e.wiki
            .collections
            .lock()
            .unwrap()
            .push(("col-de".into(), "Kitchen 1st floor (old)".into()));
        for (id, title) in [
            ("rules-de", "Old rules of kitchen 1"),
            ("decisions-de", "Old decisions of kitchen 1"),
            ("template-de", "Old template of kitchen 1"),
        ] {
            e.wiki
                .create_doc(id, "col-de", None, title, "Old text")
                .await
                .unwrap();
        }
        {
            // the decision list was written in since
            let mut docs = e.wiki.docs.lock().unwrap();
            let (d, _) = docs
                .iter_mut()
                .find(|(d, _)| d.id == "decisions-de")
                .unwrap();
            d.text = "Our own decisions".into();
            d.untouched = false;
        }
        {
            let mut st = e.state.lock().await;
            let ps = st.plenums.entry("kitchen-floor-1".into()).or_default();
            ps.collection_id = Some("col-de".into());
            ps.rules_doc_id = Some("rules-de".into());
            ps.decisions_doc_id = Some("decisions-de".into());
            ps.template_doc_id = Some("template-de".into());
            // checked a moment ago by the bot before the restart: checked again at once
            ps.area_checked = Some(at(2026, 10, 20, 11, 55));
        }
        e.tick(at(2026, 10, 20, 12, 0)).await;
        let cols = e.wiki.collections.lock().unwrap().clone();
        assert_eq!(
            cols,
            vec![("col-de".to_owned(), "Kitchen 1st floor".to_owned())]
        );
        let rules = e.wiki.by_title("Kitchen plenum 1st floor: Rules").unwrap();
        assert_eq!(rules.id, "rules-de");
        assert_eq!(rules.text, protocol::rules_doc_text(&kitchen()));
        let template = e
            .wiki
            .by_title("Kitchen plenum 1st floor: Template")
            .unwrap();
        assert_eq!(template.id, "template-de");
        assert_eq!(template.text, protocol::DEFAULT_TEMPLATE);
        let decisions = e
            .wiki
            .by_title("Kitchen plenum 1st floor: Decisions")
            .unwrap();
        assert_eq!(decisions.id, "decisions-de");
        assert_eq!(decisions.text, "Our own decisions");
        assert!(e.wiki.by_title("Old rules of kitchen 1").is_none());
    }

    #[tokio::test]
    async fn protocols_are_filed_under_the_minutes() {
        let t = setup(vec![plenum("house", "House plenum", "!house:x")]);
        let e = &t.engine;
        e.tick(at(2026, 10, 29, 19, 0)).await; // prepares the 1.11.
        let protocols = e.wiki.by_title("House plenum: Minutes").unwrap();
        let new = e.wiki.by_title("House plenum 2026-11-01").unwrap();
        assert_eq!(new.parent_id.as_deref(), Some(protocols.id.as_str()));
    }

    #[tokio::test]
    async fn people_mentioned_in_the_minutes_are_mentioned_in_the_chat() {
        let t = setup(vec![plenum("house", "House plenum", "!house:x")]);
        let e = &t.engine;
        let person = |username: &str, name: &str, account: &str| {
            let house: OwnedUserId = account.try_into().unwrap();
            DirPerson {
                id: format!("u-{username}"),
                username: username.into(),
                name: name.into(),
                active: true,
                accounts: vec![house.clone()],
                groups: Default::default(),
                house: Some(house),
                links: Vec::new(),
                choice: Choice::None,
                stay_until: None,
            }
        };
        e.set_people(&People::new(vec![
            person("ben", "Ben Berg", "@ben:house.org"),
            person("anna", "Anna", "@anna:house.org"),
            person("anna2", "Anna", "@anna2:house.org"), // two Annas: neither is pinged
        ]));
        e.tick(at(2026, 10, 29, 19, 0)).await;
        {
            let mut docs = e.wiki.docs.lock().unwrap();
            let (d, _) = docs
                .iter_mut()
                .find(|(d, _)| d.title == "House plenum 2026-11-01")
                .unwrap();
            d.text.push_str(
                "\n- [ ] @[Ben Berg](mention://m1/user/u1): Tonne kaufen\n\
                 - [ ] @[Anna](mention://m2/user/u2): Liste\n",
            );
        }
        e.tick(at(2026, 11, 2, 9, 0)).await; // the result box
        e.tick(at(2026, 11, 2, 9, 1)).await; // closed
        let summary = e
            .chat
            .texts()
            .into_iter()
            .find(|t| t.contains("are closed"))
            .expect("the summary");
        assert!(
            summary.contains("[Ben Berg](https://matrix.to/#/@ben:house.org): Tonne kaufen"),
            "{summary}"
        );
        assert!(summary.contains("@Anna: Liste"), "{summary}");
        let pinged = e.chat.pinged.lock().unwrap().clone();
        assert_eq!(pinged.len(), 1);
        assert_eq!(pinged[0].1, vec!["@ben:house.org".to_owned()]);
        // the wiki keeps the mention as it was: the next minutes mention Ben again
        let st = e.state.lock().await;
        let carried: Vec<&str> = st.plenums["house"]
            .open_tasks
            .iter()
            .map(|c| c.text.as_str())
            .collect();
        assert!(
            carried.contains(&"@[Ben Berg](mention://m1/user/u1): Tonne kaufen"),
            "{carried:?}"
        );
    }

    #[tokio::test]
    async fn open_tasks_come_along_to_the_next_protocol_and_the_overview() {
        let t = setup(vec![plenum("house", "House plenum", "!house:x")]);
        let e = &t.engine;
        e.tick(at(2026, 10, 29, 19, 0)).await;
        {
            let mut docs = e.wiki.docs.lock().unwrap();
            let (d, _) = docs
                .iter_mut()
                .find(|(d, _)| d.title == "House plenum 2026-11-01")
                .unwrap();
            d.text.push_str(
                "\nDecision: Kompost kommt.\n- [ ] Ben: Tonne kaufen\n- [x] Cara: Liste\n",
            );
        }
        e.tick(at(2026, 11, 2, 9, 0)).await; // closed
        let o = e.wiki.by_title("House plenum: Overview").unwrap();
        assert!(
            o.text
                .contains("* Ben: Tonne kaufen (since the [plenum on 1 Nov 2026]"),
            "{}",
            o.text
        );
        assert!(
            o.text.contains("* Kompost kommt. ([1 Nov 2026]"),
            "{}",
            o.text
        );
        assert!(!o.text.contains("Cara: Liste"));
        // the next meeting (3rd Sunday? the rule here: 1st Sunday): 6.12., prepared 3.12.
        e.tick(at(2026, 12, 3, 19, 0)).await;
        let next = e
            .wiki
            .by_title("House plenum 2026-12-06")
            .expect("next protocol");
        assert!(
            next.text
                .contains("- [ ] Ben: Tonne kaufen (since 1 Nov 2026)"),
            "{}",
            next.text
        );
        assert!(!next.text.contains("Cara"));
    }

    #[tokio::test]
    async fn a_meeting_moved_in_the_calendar_keeps_its_protocol() {
        let t = setup(vec![plenum("house", "House plenum", "!house:x")]);
        let e = &t.engine;
        e.tick(at(2026, 10, 29, 19, 0)).await; // 1.11. prepared and announced
        assert_eq!(e.chat.texts().len(), 1);
        let doc = e.wiki.by_title("House plenum 2026-11-01").unwrap();
        // the same occurrence a week later at 18:00 (RECURRENCE-ID: the ID stays)
        let move_to = |l: &mut Vec<Occurrence>| {
            for o in l.iter_mut().filter(|o| o.id == "house-series#2026-11-01") {
                *o = occ(&o.id, "2026-11-08 18:00");
            }
        };
        calendar(e, "house", move_to, at(2026, 10, 30, 9, 0)).await;
        e.tick(at(2026, 10, 30, 9, 0)).await;
        assert!(e.wiki.by_title("House plenum 2026-11-01").is_none());
        let moved = e.wiki.by_title("House plenum 2026-11-08").expect("renamed");
        assert_eq!(moved.id, doc.id, "the same protocol");
        // the room knew the old date: told at once (not only prepare_days before), saying
        // from when; nothing at the old time, nothing again when it is due
        let texts = e.chat.texts();
        assert_eq!(texts.len(), 2, "{texts:?}");
        assert!(
            texts[1].contains("8 November 2026 at 18:00")
                && texts[1].contains("moved from Sunday, 1 November 2026"),
            "{}",
            texts[1]
        );
        e.tick(at(2026, 11, 1, 18, 30)).await;
        e.tick(at(2026, 11, 5, 18, 0)).await;
        assert_eq!(e.chat.texts().len(), 2);
        // moved once more: from the date the room was told last
        calendar(
            e,
            "house",
            |l| {
                for o in l.iter_mut().filter(|o| o.id == "house-series#2026-11-01") {
                    *o = occ(&o.id, "2026-11-08 18:30");
                }
            },
            at(2026, 11, 5, 18, 1),
        )
        .await;
        e.tick(at(2026, 11, 5, 18, 1)).await;
        let texts = e.chat.texts();
        assert_eq!(texts.len(), 3, "{texts:?}");
        assert!(
            texts[2].contains("at 18:30")
                && texts[2].contains("moved from Sunday, 8 November 2026 at 18:00"),
            "{}",
            texts[2]
        );
        e.tick(at(2026, 11, 8, 18, 0)).await;
        assert!(e.chat.texts()[3].contains("starts at 18:30"));
        let a = e
            .command(&room(), &anna(), cmd("!plenum"), at(2026, 11, 8, 17, 31))
            .await;
        assert!(
            a.contains("8 November 2026 at 18:30") && a.contains(&moved.url),
            "{a}"
        );
    }

    // ── The rooms' members (mxbot-common `membership`) ───────────────────────────

    /// Someone in the directory who has not chosen an account: house account `@<name>:x`.
    fn unchosen(name: &str, groups: &[&str]) -> DirPerson {
        let house: OwnedUserId = format!("@{name}:x").as_str().try_into().unwrap();
        DirPerson {
            id: format!("u-{name}"),
            username: name.into(),
            name: String::new(),
            active: true,
            accounts: vec![house.clone()],
            groups: groups.iter().map(|g| (*g).to_owned()).collect(),
            house: Some(house),
            links: Vec::new(),
            choice: Choice::None,
            stay_until: None,
        }
    }

    /// Someone who chose their house account.
    fn resident(name: &str, groups: &[&str]) -> DirPerson {
        let mut p = unchosen(name, groups);
        p.choice = Choice::Account(p.accounts[0].clone());
        p
    }

    /// `p` with a link, chosen or not.
    fn linked(mut p: DirPerson, account: &str, chosen: bool) -> DirPerson {
        let id: OwnedUserId = account.try_into().unwrap();
        p.accounts.push(id.clone());
        p.links.push(Link { id: id.clone() });
        if chosen {
            p.choice = Choice::Account(id);
        }
        p
    }

    /// A read of the directory at `at` in which `groups` exist.
    fn read(people: Vec<DirPerson>, groups: &[&str], at: DateTime<Utc>) -> Snapshot {
        Snapshot {
            people: People::new(people),
            groups: groups.iter().map(|g| (*g).to_owned()).collect(),
            read_at: at.into(),
        }
    }

    const GROUPS: [&str; 5] = [
        "residents",
        "former-residents",
        "kitchen-floor-1",
        "kitchen-floor-3",
        "finance",
    ];

    /// A read in which every group of the house exists.
    fn house_read(people: Vec<DirPerson>, at: DateTime<Utc>) -> Snapshot {
        read(people, &GROUPS, at)
    }

    async fn sync(e: &Engine<FakeWiki, FakeChat>, s: Snapshot, now: DateTime<Utc>) {
        e.sync_members_after(Ok(s), now).await.unwrap();
    }

    fn managing(plenums: Vec<Plenum>) -> Engine<FakeWiki, FakeChat> {
        let mut e = setup(plenums).engine;
        e.me = Some("@plenum-bot:x".try_into().unwrap());
        e
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
                    joined: true,
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
        let people = || {
            vec![
                resident("anna", &["kitchen-floor-1"]),
                resident("ben", &["kitchen-floor-1"]),
                resident("ghost", &["kitchen-floor-1"]),
            ]
        };
        sync(
            &e,
            house_read(people(), at(2026, 10, 22, 10, 0)),
            at(2026, 10, 22, 10, 0),
        )
        .await;
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
        let now = at(2026, 10, 22, 10, 15);
        sync(&e, house_read(people(), now), now).await;
        assert!(!members_of(&e, "!k1:x").contains("@ben:x"));
        e.chat.unknown.lock().unwrap().clear();
        let now = at(2026, 10, 22, 12, 0);
        sync(&e, house_read(people(), now), now).await;
        assert!(!members_of(&e, "!k1:x").contains("@ghost:x"));
        let now = at(2026, 10, 23, 11, 0);
        sync(&e, house_read(people(), now), now).await;
        assert!(members_of(&e, "!k1:x").contains("@ghost:x"));
        // a group the directory does not know: the room is left alone
        let before = e.chat.members.lock().unwrap().len();
        let now = at(2026, 10, 23, 12, 0);
        sync(
            &e,
            read(vec![resident("x", &["residents"])], &["residents"], now),
            now,
        )
        .await;
        assert_eq!(e.chat.members.lock().unwrap().len(), before);
    }

    fn in_room(room: &str, id: &str, admin: bool) -> (String, Member) {
        (
            room.to_owned(),
            Member {
                id: id.try_into().unwrap(),
                admin,
                joined: true,
            },
        )
    }

    fn members_of(e: &Engine<FakeWiki, FakeChat>, room: &str) -> BTreeSet<String> {
        e.chat
            .members
            .lock()
            .unwrap()
            .iter()
            .filter(|(r, _)| r == room)
            .map(|(_, m)| m.id.to_string())
            .collect()
    }

    fn set_of(ids: &[&str]) -> BTreeSet<String> {
        ids.iter().map(|s| (*s).to_owned()).collect()
    }

    fn removed_from(e: &Engine<FakeWiki, FakeChat>, room: &str) -> Vec<String> {
        e.chat
            .removed
            .lock()
            .unwrap()
            .iter()
            .filter(|(r, _, _)| r == room)
            .map(|(_, u, _)| u.clone())
            .collect()
    }

    fn admin_notes(e: &Engine<FakeWiki, FakeChat>) -> Vec<String> {
        e.chat
            .admin_notes
            .lock()
            .unwrap()
            .iter()
            .map(|(_, t)| t.clone())
            .collect()
    }

    #[tokio::test]
    async fn an_emptied_group_empties_its_room_but_for_the_admins_and_the_bots() {
        let t = setup(vec![kitchen(), finance()]);
        let mut e = t.engine;
        e.me = Some("@plenum-bot:x".try_into().unwrap());
        e.keep = HashSet::from([
            "@calendar-bot:x".try_into().unwrap(),
            "@schnick:matrix.org".try_into().unwrap(),
        ]);
        e.chat.members.lock().unwrap().extend([
            in_room("!k1:x", "@plenum-bot:x", true),
            in_room("!k1:x", "@calendar-bot:x", false),
            in_room("!k1:x", "@nick:x", true),
            in_room("!k1:x", "@schnick:matrix.org", false),
            in_room("!k1:x", "@anna:x", false),
            in_room("!k1:x", "@eve:x", false),
            in_room("!fin:x", "@plenum-bot:x", true),
            in_room("!fin:x", "@ben:x", false),
        ]);
        // the last members left the kitchen's group: it exists, with nobody in it
        let now = at(2026, 10, 22, 10, 0);
        sync(
            &e,
            house_read(vec![resident("ben", &["finance"])], now),
            now,
        )
        .await;
        assert_eq!(
            members_of(&e, "!k1:x"),
            set_of(&[
                "@plenum-bot:x",
                "@calendar-bot:x",
                "@nick:x",
                "@schnick:matrix.org"
            ])
        );
        // only that room's members: the finance room is as it was
        assert_eq!(
            members_of(&e, "!fin:x"),
            set_of(&["@plenum-bot:x", "@ben:x"])
        );
        let removed = e.chat.removed.lock().unwrap().clone();
        assert_eq!(removed.len(), 2, "{removed:?}");
        assert!(removed.iter().all(|(r, _, _)| r == "!k1:x"));
        assert!(e.state.lock().await.rooms["!k1:x"].invited.is_empty());
        // someone joins the group again: invited again
        let now = at(2026, 10, 22, 10, 15);
        let people = vec![
            resident("anna", &["kitchen-floor-1"]),
            resident("ben", &["finance"]),
        ];
        sync(&e, house_read(people, now), now).await;
        assert!(members_of(&e, "!k1:x").contains("@anna:x"));
    }

    #[tokio::test]
    async fn an_empty_house_group_changes_no_room() {
        let mut house = plenum("house", "House plenum", "!house:x");
        house.group = Some("residents".into());
        let e = managing(vec![kitchen(), house]);
        e.chat.members.lock().unwrap().extend([
            in_room("!house:x", "@plenum-bot:x", true),
            in_room("!house:x", "@anna:x", false),
            in_room("!k1:x", "@plenum-bot:x", true),
            in_room("!k1:x", "@anna:x", false),
        ]);
        let before = e.chat.members.lock().unwrap().clone();
        // a broken directory: residents exists with nobody in it
        let now = at(2026, 10, 22, 10, 0);
        let why = e
            .sync_members_after(
                Ok(house_read(
                    vec![resident("cara", &["kitchen-floor-1"])],
                    now,
                )),
                now,
            )
            .await
            .unwrap_err();
        assert!(why.contains("residents"), "{why}");
        assert_eq!(*e.chat.members.lock().unwrap(), before);
        assert!(e.chat.removed.lock().unwrap().is_empty());
        assert!(e.state.lock().await.rooms.is_empty());
    }

    #[tokio::test]
    async fn a_failed_or_stale_directory_read_changes_nothing() {
        let e = managing(vec![kitchen(), plenum("house", "House plenum", "!house:x")]);
        e.chat.members.lock().unwrap().extend([
            in_room("!k1:x", "@plenum-bot:x", true),
            in_room("!k1:x", "@anna:x", false),
            in_room("!k1:x", "@eve:x", false),
        ]);
        let before = e.chat.members.lock().unwrap().clone();
        let now = at(2026, 10, 22, 10, 0);
        let why = e
            .sync_members_after(Err(anyhow::anyhow!("reading the directory: 403")), now)
            .await
            .unwrap_err();
        assert!(why.contains("403"), "{why}");
        // a read older than stale_after (60 min): nothing either
        let people = || {
            vec![
                resident("anna", &["kitchen-floor-1"]),
                resident("ben", &["kitchen-floor-1"]),
            ]
        };
        let old = house_read(people(), now - Duration::minutes(61));
        assert!(e
            .sync_members(&old, now)
            .await
            .unwrap_err()
            .contains("61 min"));
        assert_eq!(*e.chat.members.lock().unwrap(), before);
        assert!(e.chat.removed.lock().unwrap().is_empty());
        assert!(e.state.lock().await.rooms.is_empty());
        // a read that worked: the same path changes the room
        let now = at(2026, 10, 22, 10, 15);
        sync(&e, house_read(people(), now), now).await;
        assert_eq!(
            members_of(&e, "!k1:x"),
            set_of(&["@plenum-bot:x", "@anna:x", "@ben:x"])
        );
    }

    #[tokio::test]
    async fn a_teams_group_counts_only_for_residents() {
        let mut house = plenum("house", "House plenum", "!house:x");
        house.group = Some("residents".into());
        let mut k = kitchen();
        k.require_group = Some("residents".into());
        let e = managing(vec![house, k]);
        e.chat.members.lock().unwrap().extend([
            in_room("!k1:x", "@plenum-bot:x", true),
            in_room("!k1:x", "@nick:x", true),
            in_room("!k1:x", "@anna:x", false),
            in_room("!k1:x", "@olaf:x", false), // moved out, still in the kitchen's group
            in_room("!house:x", "@plenum-bot:x", true),
            in_room("!house:x", "@anna:x", false),
            in_room("!house:x", "@olaf:x", false),
        ]);
        let people = vec![
            resident("anna", &["residents", "kitchen-floor-1"]),
            resident("cara", &["residents", "kitchen-floor-1"]),
            resident("olaf", &["former-residents", "kitchen-floor-1"]),
            resident("ben", &["kitchen-floor-1"]),
        ];
        // without the house group in the read, the kitchen and the house are left alone
        let now = at(2026, 10, 22, 9, 0);
        let partial = read(
            vec![resident("anna", &["kitchen-floor-1"])],
            &["kitchen-floor-1"],
            now,
        );
        sync(&e, partial, now).await;
        assert!(e.chat.removed.lock().unwrap().is_empty());
        let now = at(2026, 10, 22, 10, 0);
        sync(&e, house_read(people, now), now).await;
        // olaf (person E) is removed, ben (not living here) is not invited, cara (lives here) is
        assert_eq!(
            members_of(&e, "!k1:x"),
            set_of(&["@plenum-bot:x", "@nick:x", "@anna:x", "@cara:x"])
        );
        assert_eq!(
            members_of(&e, "!house:x"),
            set_of(&["@plenum-bot:x", "@anna:x", "@cara:x"])
        );
        let removed = e.chat.removed.lock().unwrap().clone();
        let kitchen_removal = removed.iter().find(|(r, _, _)| r == "!k1:x").unwrap();
        assert_eq!(kitchen_removal.1, "@olaf:x");
        assert!(
            kitchen_removal.2.contains("kitchen-floor-1")
                && kitchen_removal.2.contains("residents"),
            "{}",
            kitchen_removal.2
        );
    }

    #[test]
    fn subtenants_count_in_the_kitchens_rooms_only() {
        let house: BTreeSet<String> = ["residents".to_owned()].into();
        let subtenants = ["subtenants".to_owned()];
        let rule = |p: &Plenum| room_rule(p, &house, &subtenants).unwrap();
        // the house plenum's room: its group, nobody else
        let r = rule(&house_with_group());
        assert_eq!((r.group.as_str(), r.require.len()), ("residents", 0));
        // a kitchen requiring the house's group: who lives here, subtenants too
        assert_eq!(rule(&kitchen3()).require, ["residents", "subtenants"]);
        // a private plenum (finance) requiring it: full members only
        let finance = Plenum {
            group: Some("finance".into()),
            require_group: Some("residents".into()),
            access: Access::Private,
            ..plenum("finance", "Finance plenum", "!fin:x")
        };
        assert_eq!(rule(&finance).require, ["residents"]);
        // a kitchen requiring another group, or none: as configured
        let guests = Plenum {
            require_group: Some("guests".into()),
            ..kitchen3()
        };
        assert_eq!(rule(&guests).require, ["guests"]);
        let open = Plenum {
            require_group: None,
            ..kitchen3()
        };
        assert!(rule(&open).require.is_empty());
        // without subtenant groups configured: as before
        assert_eq!(
            room_rule(&kitchen3(), &house, &[]).unwrap().require,
            ["residents"]
        );
        // no group, no rule
        assert!(room_rule(&plenum("x", "X", "!x:x"), &house, &subtenants).is_none());
    }

    #[tokio::test]
    async fn a_subtenant_is_invited_to_their_kitchen_and_never_to_the_house_room() {
        let mut e = managing(vec![house_with_group(), kitchen3()]);
        e.subtenant_groups = vec!["subtenants".into()];
        e.chat.members.lock().unwrap().extend([
            in_room("!k3:x", "@plenum-bot:x", true),
            in_room("!house:x", "@plenum-bot:x", true),
            // someone invited the subtenant into the house's room by hand
            in_room("!house:x", "@sub:x", false),
        ]);
        let people = |sub_groups: &[&str]| {
            vec![
                resident("anna", &["residents", "kitchen-floor-3"]),
                resident("sub", sub_groups),
                resident("finn", &["residents", "finance"]),
            ]
        };
        let groups: Vec<&str> = GROUPS.iter().copied().chain(["subtenants"]).collect();
        let now = at(2026, 10, 22, 10, 0);
        sync(
            &e,
            read(people(&["subtenants", "kitchen-floor-3"]), &groups, now),
            now,
        )
        .await;
        assert_eq!(
            members_of(&e, "!k3:x"),
            set_of(&["@plenum-bot:x", "@anna:x", "@sub:x"])
        );
        assert_eq!(
            members_of(&e, "!house:x"),
            set_of(&["@plenum-bot:x", "@anna:x", "@finn:x"])
        );
        // the subtenants group missing from the read: the kitchen is left alone
        let later = at(2026, 10, 22, 10, 15);
        sync(
            &e,
            read(people(&["kitchen-floor-3"]), &GROUPS, later),
            later,
        )
        .await;
        assert!(members_of(&e, "!k3:x").contains("@sub:x"));
        // moved out (subtenants → former-residents): out of the kitchen too
        let later = at(2026, 10, 22, 10, 30);
        sync(
            &e,
            read(
                people(&["former-residents", "kitchen-floor-3"]),
                &groups,
                later,
            ),
            later,
        )
        .await;
        assert_eq!(
            members_of(&e, "!k3:x"),
            set_of(&["@plenum-bot:x", "@anna:x"])
        );
        let removed = e.chat.removed.lock().unwrap().clone();
        let out = removed.iter().find(|(r, _, _)| r == "!k3:x").unwrap();
        assert!(
            out.2.contains("in none of residents, subtenants"),
            "{}",
            out.2
        );
    }

    #[test]
    fn a_subtenant_without_a_kitchen_yet_may_ask_for_a_link_and_write_to_the_bot() {
        let id = |s: &str| -> OwnedUserId { s.try_into().unwrap() };
        let mut e = managing(vec![house_with_group(), kitchen3()]);
        let only_subtenants = |e: &Engine<FakeWiki, FakeChat>| {
            let people = People::new(vec![
                resident("anna", &["residents"]),
                // just moved in: the kitchen's group comes later
                resident("sub", &["subtenants"]),
                resident("guest", &["guests"]),
            ]);
            let groups = e.managed_groups();
            // who may write to the bot (`DmAccounts::follow`) and ask for a code
            // (`Directory::link_requests`: active people in one of the groups)
            let known = mxbot_common::rooms::known_accounts(&people, &groups);
            let mut asks: Vec<String> = people
                .active()
                .filter(|p| p.groups.iter().any(|g| groups.contains(g)))
                .map(|p| p.username.clone())
                .collect();
            asks.sort();
            (known, asks)
        };
        let (known, asks) = only_subtenants(&e);
        assert!(
            !known.contains(&id("@sub:x")),
            "no subtenant groups configured"
        );
        assert_eq!(asks, ["anna"]);
        e.subtenant_groups = vec!["subtenants".into()];
        let (known, asks) = only_subtenants(&e);
        assert!(known.contains(&id("@sub:x")) && known.contains(&id("@anna:x")));
        assert!(!known.contains(&id("@guest:x")));
        assert_eq!(asks, ["anna", "sub"]);
        // the members' read asks for the same groups
        assert!(e.managed_groups().contains("subtenants"));
    }

    #[tokio::test]
    async fn the_admins_hear_of_the_end_of_a_stay_once_per_stage() {
        let mut t = setup(vec![house_with_group(), kitchen3()]);
        t.engine.me = Some("@plenum-bot:x".try_into().unwrap());
        t.engine.subtenant_groups = vec!["subtenants".into()];
        let groups: Vec<&str> = GROUPS.iter().copied().chain(["subtenants"]).collect();
        let people = |until: Option<NaiveDate>, active: bool| {
            let mut sub = resident("sub", &["subtenants", "kitchen-floor-3"]);
            sub.name = "Sam Sub".into();
            sub.stay_until = until;
            if !active {
                sub.active = false;
                sub.groups.clear();
            }
            vec![resident("anna", &["residents", "kitchen-floor-3"]), sub]
        };
        let nov30 = NaiveDate::from_ymd_opt(2026, 11, 30);
        let stays = |e: &Engine<FakeWiki, FakeChat>| -> Vec<String> {
            e.chat
                .admin_notes
                .lock()
                .unwrap()
                .iter()
                .filter(|(k, _)| k.starts_with("members-stay:"))
                .map(|(k, _)| k.clone())
                .collect()
        };
        // more than 14 days before: nothing
        let now = at(2026, 11, 15, 12, 0);
        sync(&t.engine, read(people(nov30, true), &groups, now), now).await;
        assert!(stays(&t.engine).is_empty());
        // 14 days before: once, however often the directory is read
        for h in [9, 10, 11] {
            let now = at(2026, 11, 16, h, 0);
            sync(&t.engine, read(people(nov30, true), &groups, now), now).await;
        }
        assert_eq!(
            stays(&t.engine),
            ["members-stay:u-sub:2026-11-30:soon:2026-11-16"]
        );
        let text = t.engine.chat.admin_notes.lock().unwrap()[0].1.clone();
        assert!(
            text.contains("sub (Sam Sub) ends in 14 days (30 Nov 2026"),
            "{text}"
        );
        // `!members` lists it
        let now = at(2026, 11, 16, 11, 0);
        let overview = t
            .engine
            .command_in(&room(), &anna(), Command::Members, now, Scope::AdminDm)
            .await;
        assert!(
            overview.contains("- sub (Sam Sub): until 30 Nov 2026 (ends in 14 days;"),
            "{overview}"
        );
        // a restart remembers it: nothing again
        restart(&mut t).await;
        t.engine.me = Some("@plenum-bot:x".try_into().unwrap());
        t.engine.subtenant_groups = vec!["subtenants".into()];
        let now = at(2026, 11, 20, 9, 0);
        sync(&t.engine, read(people(nov30, true), &groups, now), now).await;
        assert!(stays(&t.engine).is_empty());
        // the day in the bot's time zone: 00:30 in Berlin is still the 29th in UTC
        let now = at(2026, 11, 30, 0, 30);
        assert_eq!(
            now.date_naive(),
            NaiveDate::from_ymd_opt(2026, 11, 29).unwrap()
        );
        sync(&t.engine, read(people(nov30, true), &groups, now), now).await;
        assert_eq!(
            stays(&t.engine),
            ["members-stay:u-sub:2026-11-30:today:2026-11-30"]
        );
        // still active two days later: overdue, once; nobody is removed for it
        for d in [2, 3] {
            let now = at(2026, 12, d, 9, 0);
            sync(&t.engine, read(people(nov30, true), &groups, now), now).await;
        }
        assert_eq!(stays(&t.engine).len(), 2);
        assert!(stays(&t.engine)[1].contains(":overdue:2026-12-02"));
        assert!(t.engine.chat.removed.lock().unwrap().is_empty());
        let now = at(2026, 12, 3, 9, 0);
        let overview = t
            .engine
            .command_in(&room(), &anna(), Command::Members, now, Scope::AdminDm)
            .await;
        assert!(overview.contains("overdue: still active"), "{overview}");
        // extended: a new date starts again (quiet until two weeks before it)
        let jan31 = NaiveDate::from_ymd_opt(2027, 1, 31);
        let now = at(2027, 1, 17, 9, 0);
        sync(&t.engine, read(people(jan31, true), &groups, now), now).await;
        assert_eq!(stays(&t.engine).len(), 3);
        assert!(stays(&t.engine)[2].contains("2027-01-31:soon"));
        // deactivated: ignored; the attribute removed: forgotten
        let now = at(2027, 1, 31, 9, 0);
        sync(&t.engine, read(people(jan31, false), &groups, now), now).await;
        assert_eq!(stays(&t.engine).len(), 3);
        sync(&t.engine, read(people(None, true), &groups, now), now).await;
        assert_eq!(stays(&t.engine).len(), 3);
        assert!(t.engine.state.lock().await.stays.is_empty());
    }

    #[tokio::test]
    async fn a_room_with_a_plenum_without_group_is_never_managed() {
        let e = managing(vec![plenum("house", "House plenum", "!house:x")]);
        e.chat
            .members
            .lock()
            .unwrap()
            .push(in_room("!house:x", "@eve:x", false));
        let now = at(2026, 10, 22, 10, 0);
        sync(
            &e,
            house_read(vec![resident("anna", &["residents"])], now),
            now,
        )
        .await;
        assert!(e.chat.removed.lock().unwrap().is_empty());
    }

    fn kitchen3() -> Plenum {
        Plenum {
            group: Some("kitchen-floor-3".into()),
            require_group: Some("residents".into()),
            access: Access::Members,
            ..plenum("kitchen-floor-3", "Kitchen plenum 3rd floor", "!k3:x")
        }
    }

    fn house_with_group() -> Plenum {
        Plenum {
            group: Some("residents".into()),
            ..plenum("house", "House plenum", "!house:x")
        }
    }

    #[tokio::test]
    async fn only_the_chosen_account_is_invited_and_the_others_stay_for_the_handover() {
        let mut e = managing(vec![house_with_group()]);
        e.ask_to_choose = true;
        e.choose_url = Some("https://login.x/flow/".into());
        // B joined with the house account on their own before choosing a link
        e.chat.members.lock().unwrap().extend([
            in_room("!house:x", "@plenum-bot:x", true),
            in_room("!house:x", "@b:x", false),
        ]);
        let people = || {
            vec![
                resident("z", &["residents"]),
                // A: only the house account, nothing chosen
                unchosen("a", &["residents"]),
                // B: chose a link
                linked(unchosen("b", &["residents"]), "@b:matrix.org", true),
                // C: two links and the house account, chose one
                linked(
                    linked(unchosen("c", &["residents"]), "@c:matrix.org", false),
                    "@c:example.org",
                    true,
                ),
            ]
        };
        let now = at(2026, 10, 22, 10, 0);
        sync(&e, house_read(people(), now), now).await;
        assert_eq!(
            members_of(&e, "!house:x"),
            set_of(&[
                "@plenum-bot:x",
                "@z:x",
                "@b:x",
                "@b:matrix.org",
                "@c:example.org"
            ])
        );
        assert!(e.chat.removed.lock().unwrap().is_empty());
        // A is asked once, in German, at the house account, with the flow's address
        let dms = asked(&e);
        assert_eq!(dms.len(), 1, "{dms:?}");
        assert_eq!(dms[0].0, "@a:x");
        assert!(
            dms[0].1.contains("https://login.x/flow/") && dms[0].1.contains("!link a"),
            "{}",
            dms[0].1
        );
        assert!(dms[0].1.contains("House plenum"), "{}", dms[0].1);
        // B hears at the chosen link that the house account goes after the handover
        let told = handover_dms(&e);
        assert_eq!(told.len(), 1, "{told:?}");
        assert_eq!(told[0].0, "@b:matrix.org");
        // C's other accounts joining on their own stay for the handover
        e.chat.members.lock().unwrap().extend([
            in_room("!house:x", "@c:matrix.org", false),
            in_room("!house:x", "@c:x", false),
        ]);
        let now = at(2026, 10, 22, 10, 15);
        sync(&e, house_read(people(), now), now).await;
        assert!(e.chat.removed.lock().unwrap().is_empty());
        assert_eq!(handover_dms(&e).len(), 2);
        // nobody is invited twice, nor asked again within two weeks
        let invited = e.chat.members.lock().unwrap().len();
        let now = at(2026, 11, 4, 10, 0);
        sync(&e, house_read(people(), now), now).await;
        assert_eq!(e.chat.members.lock().unwrap().len(), invited);
        assert_eq!(asked(&e).len(), 1);
        assert_eq!(handover_dms(&e).len(), 2);
        // two weeks later: asked again, three times at most
        for now in [
            at(2026, 11, 5, 10, 0),
            at(2026, 11, 19, 10, 0),
            at(2026, 12, 3, 10, 0),
        ] {
            sync(&e, house_read(people(), now), now).await;
        }
        assert_eq!(asked(&e).len(), 3);
        assert_eq!(e.state.lock().await.people.asked["u-a"].times, 3);
    }

    /// The requests to choose an account: (account, text).
    fn asked(e: &Engine<FakeWiki, FakeChat>) -> Vec<(String, String)> {
        dms_keyed(e, "members-ask:")
    }

    /// The notices of a handover: (account, text).
    fn handover_dms(e: &Engine<FakeWiki, FakeChat>) -> Vec<(String, String)> {
        dms_keyed(e, "members-handover:")
    }

    fn dms_keyed(e: &Engine<FakeWiki, FakeChat>, prefix: &str) -> Vec<(String, String)> {
        e.chat
            .dms
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, _, key)| key.starts_with(prefix))
            .map(|(to, text, _)| (to.clone(), text.clone()))
            .collect()
    }

    /// `account` accepts its invitation to `room`.
    fn accept(e: &Engine<FakeWiki, FakeChat>, room: &str, account: &str) {
        for (r, m) in e.chat.members.lock().unwrap().iter_mut() {
            if r == room && m.id == account {
                m.joined = true;
            }
        }
    }

    async fn overview(e: &Engine<FakeWiki, FakeChat>, now: DateTime<Utc>) -> String {
        let admin = Sender {
            id: "@admin:x".try_into().unwrap(),
            name: "Admin".into(),
        };
        let dm: OwnedRoomId = "!admin-dm:x".try_into().unwrap();
        e.command_in(&dm, &admin, cmd("!members"), now, Scope::AdminDm)
            .await
    }

    #[tokio::test]
    async fn without_ask_to_choose_nobody_is_written_to() {
        let e = managing(vec![house_with_group()]);
        let now = at(2026, 10, 22, 10, 0);
        sync(
            &e,
            house_read(vec![unchosen("a", &["residents"])], now),
            now,
        )
        .await;
        assert!(e.chat.dms.lock().unwrap().is_empty());
        assert!(members_of(&e, "!house:x").is_empty());
    }

    #[tokio::test]
    async fn person_h_switching_keeps_the_old_account_for_the_handover_and_back_again() {
        let mut e = managing(vec![house_with_group()]);
        e.choose_url = Some("https://login.x/flow/".into());
        e.chat
            .members
            .lock()
            .unwrap()
            .push(in_room("!house:x", "@h:x", false));
        let before = linked(resident("h", &["residents"]), "@h:matrix.org", false);
        let after = linked(unchosen("h", &["residents"]), "@h:matrix.org", true);
        let now = at(2026, 10, 22, 10, 0);
        sync(&e, house_read(vec![before.clone()], now), now).await;
        assert_eq!(members_of(&e, "!house:x"), set_of(&["@h:x"]));
        assert!(handover_dms(&e).is_empty());
        // the switch: the link is invited, the house account stays, and they hear until when
        // (30 days at most while the link has not joined)
        let switched = at(2026, 10, 22, 10, 15);
        sync(&e, house_read(vec![after.clone()], switched), switched).await;
        assert_eq!(
            members_of(&e, "!house:x"),
            set_of(&["@h:x", "@h:matrix.org"])
        );
        let told = handover_dms(&e);
        assert_eq!(told.len(), 1, "{told:?}");
        assert_eq!(told[0].0, "@h:matrix.org");
        for part in [
            "Dein Konto für die Hausräume ist jetzt @h:matrix.org",
            "@h:x bleibt noch bis spätestens 21.11.2026, 09:15 Uhr in House plenum",
            "Nimm die Einladung an @h:matrix.org an: 7 Tage danach",
            "„Matrix-Konten“ (https://login.x/flow/)",
        ] {
            assert!(told[0].1.contains(part), "{part}: {}", told[0].1);
        }
        let a = overview(&e, switched).await;
        assert!(
            a.contains(
                "- House plenum: @h:x → @h:matrix.org, until 21 Nov 2026 09:15 (invited, not \
                 joined yet)"
            ),
            "{a}"
        );
        // told once; the link joins three days later: a week from then
        let now = at(2026, 10, 25, 12, 0);
        sync(&e, house_read(vec![after.clone()], now), now).await;
        accept(&e, "!house:x", "@h:matrix.org");
        sync(&e, house_read(vec![after.clone()], now), now).await;
        assert_eq!(handover_dms(&e).len(), 1);
        assert!(overview(&e, now).await.contains("until 1 Nov 2026 12:00"));
        let now = at(2026, 11, 1, 11, 59);
        sync(&e, house_read(vec![after.clone()], now), now).await;
        assert!(e.chat.removed.lock().unwrap().is_empty());
        let now = at(2026, 11, 1, 12, 0);
        sync(&e, house_read(vec![after.clone()], now), now).await;
        assert_eq!(members_of(&e, "!house:x"), set_of(&["@h:matrix.org"]));
        let removed = e.chat.removed.lock().unwrap().clone();
        assert_eq!(removed.len(), 1);
        assert_eq!(
            removed[0].2,
            "Ersetzt durch dein gewähltes Konto @h:matrix.org (Einstellungen → Matrix-Konten)"
        );
        assert!(!overview(&e, now).await.contains("Handovers"));
        // back to the house account: invited again, the link hands over, told again
        let now = at(2026, 11, 3, 10, 0);
        sync(&e, house_read(vec![before.clone()], now), now).await;
        assert_eq!(
            members_of(&e, "!house:x"),
            set_of(&["@h:x", "@h:matrix.org"])
        );
        let told = handover_dms(&e);
        assert_eq!(told.len(), 2, "{told:?}");
        assert_eq!(told[1].0, "@h:x");
        assert!(told[1].1.contains("@h:matrix.org bleibt"), "{}", told[1].1);
        assert!(told[1].1.contains("Nimm die Einladung an"), "{}", told[1].1);
        // they leave with both: neither is invited again (no loop)
        e.chat.members.lock().unwrap().clear();
        for minutes in [15, 30] {
            let now = at(2026, 11, 3, 10, minutes);
            sync(&e, house_read(vec![before.clone()], now), now).await;
        }
        assert!(members_of(&e, "!house:x").is_empty());
        assert_eq!(e.chat.removed.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_handover_goes_on_after_a_restart_and_is_told_once() {
        let mut t = setup(vec![house_with_group()]);
        t.engine.me = Some("@plenum-bot:x".try_into().unwrap());
        // chose the link before this image; both accounts are in the room
        t.engine.chat.members.lock().unwrap().extend([
            in_room("!house:x", "@h:x", false),
            in_room("!house:x", "@h:matrix.org", false),
        ]);
        let people = || vec![linked(unchosen("h", &["residents"]), "@h:matrix.org", true)];
        let start = at(2026, 10, 22, 10, 0);
        sync(&t.engine, house_read(people(), start), start).await;
        assert!(t.engine.chat.removed.lock().unwrap().is_empty());
        let told = handover_dms(&t.engine);
        assert_eq!(told.len(), 1);
        // the link is in the room already: only the date, no word of invitations
        assert!(
            told[0]
                .1
                .contains("@h:x bleibt noch bis spätestens 29.10.2026, 09:00 Uhr")
                && !told[0].1.contains("Einladung"),
            "{}",
            told[0].1
        );
        let members = t.engine.chat.members.lock().unwrap().clone();
        restart(&mut t).await;
        t.engine.me = Some("@plenum-bot:x".try_into().unwrap());
        *t.engine.chat.members.lock().unwrap() = members;
        let now = at(2026, 10, 25, 10, 0);
        sync(&t.engine, house_read(people(), now), now).await;
        assert!(t.engine.chat.removed.lock().unwrap().is_empty());
        assert!(t.engine.chat.dms.lock().unwrap().is_empty());
        let now = at(2026, 10, 29, 10, 0);
        sync(&t.engine, house_read(people(), now), now).await;
        assert_eq!(removed_from(&t.engine, "!house:x"), ["@h:x"]);
    }

    /// One round, as the members' loop runs it, of the house room with `people`.
    async fn house_round(
        e: &Engine<FakeWiki, FakeChat>,
        people: Vec<DirPerson>,
        now: DateTime<Utc>,
    ) {
        sync(e, house_read(people, now), now).await;
    }

    #[tokio::test]
    async fn the_upgrade_invites_the_chosen_account_again_and_says_so() {
        let e = managing(vec![house_with_group()]);
        // an older bot invited the link and it declined: only the house account is here
        e.state
            .lock()
            .await
            .rooms
            .entry("!house:x".into())
            .or_default()
            .invited
            .extend(["@h:x".to_owned(), "@h:matrix.org".to_owned()]);
        e.chat
            .members
            .lock()
            .unwrap()
            .push(in_room("!house:x", "@h:x", false));
        let h = || vec![linked(unchosen("h", &["residents"]), "@h:matrix.org", true)];
        let start = at(2026, 10, 22, 10, 0);
        house_round(&e, h(), start).await;
        assert_eq!(
            members_of(&e, "!house:x"),
            set_of(&["@h:x", "@h:matrix.org"]),
            "invited once more"
        );
        // the message says what is: an invitation is waiting
        let told = handover_dms(&e);
        assert_eq!(told.len(), 1, "{told:?}");
        assert!(
            told[0]
                .1
                .contains("Nimm die Einladung an @h:matrix.org an: 7 Tage danach"),
            "{}",
            told[0].1
        );
        assert!(!told[0].1.contains("weder drin"), "{}", told[0].1);
        // declined once more: not invited again, and the house account stays past the end
        e.chat
            .members
            .lock()
            .unwrap()
            .retain(|(_, m)| m.id != "@h:matrix.org");
        for days in [1, 31, 60] {
            let now = start + Duration::days(days);
            house_round(&e, h(), now).await;
        }
        assert_eq!(members_of(&e, "!house:x"), set_of(&["@h:x"]));
        assert!(e.chat.removed.lock().unwrap().is_empty());
        let a = overview(&e, start + Duration::days(60)).await;
        assert!(
            a.contains(
                "@h:x → @h:matrix.org, until 21 Nov 2026 09:00 (declined or left after the \
                 handover's invitation, past the end: the old account stays until it is)"
            ),
            "{a}"
        );
    }

    #[tokio::test]
    async fn a_chosen_account_that_cannot_be_invited_never_costs_the_old_one() {
        let e = managing(vec![house_with_group()]);
        // every invitation of the link to the room fails (its server may not join, say)
        e.chat
            .uninvitable
            .lock()
            .unwrap()
            .push("@h:matrix.org".into());
        e.chat
            .members
            .lock()
            .unwrap()
            .push(in_room("!house:x", "@h:x", false));
        let h = || vec![linked(unchosen("h", &["residents"]), "@h:matrix.org", true)];
        let start = at(2026, 10, 22, 10, 0);
        house_round(&e, h(), start).await;
        let told = handover_dms(&e);
        assert_eq!(told.len(), 1, "{told:?}");
        for part in [
            "In House plenum ist @h:matrix.org gerade weder drin noch eingeladen",
            "dort bleibt @h:x, bis @h:matrix.org im Raum ist",
        ] {
            assert!(told[0].1.contains(part), "{part}: {}", told[0].1);
        }
        assert!(!told[0].1.contains("Nimm die Einladung"), "{}", told[0].1);
        // tried again a day apart, past the end too: the house account stays
        for days in [1, 2, 31, 45] {
            house_round(&e, h(), start + Duration::days(days)).await;
        }
        assert_eq!(members_of(&e, "!house:x"), set_of(&["@h:x"]));
        assert!(e.chat.removed.lock().unwrap().is_empty());
        let a = overview(&e, start + Duration::days(45)).await;
        assert!(
            a.contains("(its invitation failed, past the end: the old account stays until it is)"),
            "{a}"
        );
        // it works at last: invited, and the house account goes in the next round
        e.chat.uninvitable.lock().unwrap().clear();
        let now = start + Duration::days(46);
        house_round(&e, h(), now).await;
        assert_eq!(
            members_of(&e, "!house:x"),
            set_of(&["@h:x", "@h:matrix.org"])
        );
        house_round(&e, h(), now + Duration::minutes(15)).await;
        assert_eq!(members_of(&e, "!house:x"), set_of(&["@h:matrix.org"]));
    }

    #[tokio::test]
    async fn many_handovers_ending_at_once_wait_for_an_admin() {
        let e = managing(vec![house_with_group()]);
        let names = ["a", "b", "c", "d", "e"];
        for n in names {
            e.chat.members.lock().unwrap().extend([
                in_room("!house:x", &format!("@{n}:x"), false),
                in_room("!house:x", &format!("@{n}:matrix.org"), false),
            ]);
        }
        let people = || {
            names
                .iter()
                .map(|n| {
                    linked(
                        unchosen(n, &["residents"]),
                        &format!("@{n}:matrix.org"),
                        true,
                    )
                })
                .collect::<Vec<_>>()
        };
        let now = at(2026, 10, 22, 10, 0);
        sync(&e, house_read(people(), now), now).await;
        assert_eq!(handover_dms(&e).len(), 5);
        let now = at(2026, 10, 29, 10, 0);
        sync(&e, house_read(people(), now), now).await;
        assert!(e.chat.removed.lock().unwrap().is_empty());
        let notes = admin_notes(&e);
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(
            notes[0].contains("5 removals held")
                && notes[0].contains("@a:x (handed over to @a:matrix.org)"),
            "{}",
            notes[0]
        );
    }

    #[tokio::test]
    async fn a_kitchen_change_moves_the_rooms() {
        let mut k1 = kitchen();
        k1.require_group = Some("residents".into());
        let e = managing(vec![house_with_group(), k1, kitchen3()]);
        e.chat.members.lock().unwrap().extend([
            in_room("!k1:x", "@d:x", false),
            in_room("!house:x", "@d:x", false),
        ]);
        let now = at(2026, 10, 22, 10, 0);
        sync(
            &e,
            house_read(vec![resident("d", &["residents", "kitchen-floor-3"])], now),
            now,
        )
        .await;
        assert_eq!(removed_from(&e, "!k1:x"), ["@d:x"]);
        assert!(removed_from(&e, "!house:x").is_empty());
        assert!(members_of(&e, "!k3:x").contains("@d:x"));
    }

    #[tokio::test]
    async fn a_deactivated_person_is_removed_and_the_admins_told_once() {
        let e = managing(vec![house_with_group(), kitchen()]);
        e.chat.members.lock().unwrap().extend([
            in_room("!house:x", "@f:x", false),
            in_room("!house:x", "@g:x", false),
            in_room("!k1:x", "@f:x", false),
        ]);
        let mut f = resident("f", &[]);
        f.active = false;
        let people = || vec![f.clone(), resident("g", &["residents"])];
        let now = at(2026, 10, 22, 10, 0);
        sync(&e, house_read(people(), now), now).await;
        assert_eq!(removed_from(&e, "!house:x"), ["@f:x"]);
        assert_eq!(removed_from(&e, "!k1:x"), ["@f:x"]);
        let notes = admin_notes(&e);
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(
            notes[0].contains("f is deactivated")
                && notes[0].contains("still active")
                && notes[0].contains("OPERATIONS.md"),
            "{}",
            notes[0]
        );
        // back in a room (re-joined by hand): removed again, the admins not told again
        e.chat
            .members
            .lock()
            .unwrap()
            .push(in_room("!house:x", "@f:x", false));
        let now = at(2026, 10, 22, 10, 15);
        sync(&e, house_read(people(), now), now).await;
        assert_eq!(removed_from(&e, "!house:x"), ["@f:x", "@f:x"]);
        assert_eq!(admin_notes(&e).len(), 1);
        assert!(e.state.lock().await.people.deactivated.contains("u-f"));
    }

    #[tokio::test]
    async fn an_unusable_choice_and_a_claimed_account_change_nothing_and_the_admins_hear_once() {
        let e = managing(vec![house_with_group()]);
        // the shared account: two people claim it, it stays where it is
        e.chat.members.lock().unwrap().extend([
            in_room("!house:x", "@shared:matrix.org", false),
            in_room("!house:x", "@i:x", false),
        ]);
        let mut i = unchosen("i", &["residents"]);
        i.choice = Choice::Account("@i:matrix.org".try_into().unwrap()); // not one of theirs
        let people = || {
            vec![
                i.clone(),
                linked(resident("j", &["residents"]), "@shared:matrix.org", false),
                linked(unchosen("k", &[]), "@shared:matrix.org", false),
            ]
        };
        for minutes in [0, 15] {
            let now = at(2026, 10, 22, 10, minutes);
            sync(&e, house_read(people(), now), now).await;
        }
        assert!(e.chat.removed.lock().unwrap().is_empty());
        assert_eq!(
            members_of(&e, "!house:x"),
            set_of(&["@shared:matrix.org", "@i:x", "@j:x"])
        );
        // once each: the account two claim, who cannot be invited, the unusable choice
        let notes = admin_notes(&e);
        assert_eq!(notes.len(), 3, "{notes:?}");
        assert!(
            notes[0].contains("@shared:matrix.org is a Matrix account of j and k"),
            "{}",
            notes[0]
        );
        assert!(
            notes[1].contains("1 person cannot be invited")
                && notes[1].contains("- i (their choice cannot be used): House plenum"),
            "{}",
            notes[1]
        );
        assert!(
            notes[2].contains("i chose @i:matrix.org") && notes[2].contains("one of their links"),
            "{}",
            notes[2]
        );
    }

    #[tokio::test]
    async fn a_claimed_account_nobody_here_belongs_to_goes() {
        // olaf moved out; the link he and ben (not in the kitchen) both claim is in the kitchen
        let mut k = kitchen();
        k.require_group = Some("residents".into());
        let e = managing(vec![house_with_group(), k]);
        e.chat.members.lock().unwrap().extend([
            in_room("!k1:x", "@shared:matrix.org", false),
            in_room("!k1:x", "@anna:x", false),
            in_room("!house:x", "@shared:matrix.org", false),
        ]);
        let people = || {
            vec![
                resident("anna", &["residents", "kitchen-floor-1"]),
                linked(
                    resident("olaf", &["former-residents", "kitchen-floor-1"]),
                    "@shared:matrix.org",
                    false,
                ),
                linked(resident("ben", &["residents"]), "@shared:matrix.org", false),
            ]
        };
        for minutes in [0, 15] {
            let now = at(2026, 10, 22, 10, minutes);
            sync(&e, house_read(people(), now), now).await;
        }
        assert_eq!(removed_from(&e, "!k1:x"), ["@shared:matrix.org"]);
        // ben belongs in the house's room: there it stays
        assert!(removed_from(&e, "!house:x").is_empty());
        let notes = admin_notes(&e);
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(notes[0].contains("of ben and olaf"), "{}", notes[0]);
    }

    #[tokio::test]
    async fn the_admins_hear_who_cannot_be_invited_when_the_list_grows() {
        let e = managing(vec![house_with_group(), kitchen()]);
        let mut people = vec![
            unchosen("a", &["residents", "kitchen-floor-1"]),
            unchosen("b", &["residents"]),
        ];
        let now = at(2026, 10, 22, 10, 0);
        sync(&e, house_read(people.clone(), now), now).await;
        let notes = admin_notes(&e);
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(
            notes[0].contains("2 people cannot be invited")
                && notes[0]
                    .contains("- a (has not chosen): House plenum, Kitchen plenum 1st floor")
                && notes[0].contains("- b (has not chosen): House plenum")
                && notes[0].contains("ask_to_choose` is off"),
            "{}",
            notes[0]
        );
        // the same list: nothing new; b chose: no news either
        let now = at(2026, 10, 22, 10, 15);
        sync(&e, house_read(people.clone(), now), now).await;
        people[1] = resident("b", &["residents"]);
        let now = at(2026, 10, 22, 10, 30);
        sync(&e, house_read(people.clone(), now), now).await;
        assert_eq!(admin_notes(&e).len(), 1);
        // c is new: the whole list again
        people.push(unchosen("c", &["residents"]));
        let now = at(2026, 10, 22, 10, 45);
        sync(&e, house_read(people.clone(), now), now).await;
        let notes = admin_notes(&e);
        assert_eq!(notes.len(), 2, "{notes:?}");
        assert!(
            notes[1].contains("2 people cannot be invited") && notes[1].contains("- c ("),
            "{}",
            notes[1]
        );
        // everyone chose: nothing; a falling back later is news again, with a new key
        let chose: Vec<DirPerson> = ["a", "b", "c"]
            .iter()
            .map(|n| resident(n, &["residents"]))
            .collect();
        let now = at(2026, 10, 22, 11, 0);
        sync(&e, house_read(chose, now), now).await;
        assert_eq!(admin_notes(&e).len(), 2);
        let now = at(2026, 10, 23, 11, 0);
        sync(
            &e,
            house_read(vec![unchosen("a", &["residents"])], now),
            now,
        )
        .await;
        assert_eq!(admin_notes(&e).len(), 3);
    }

    #[tokio::test]
    async fn a_recurring_notice_gets_a_new_key() {
        // like the homeserver, the fake chat drops a key it has seen: each episode is told
        let e = managing(vec![house_with_group()]);
        let mut f = resident("f", &[]);
        f.active = false;
        let g = resident("g", &["residents"]);
        e.chat.members.lock().unwrap().extend([
            in_room("!house:x", "@f:x", false),
            in_room("!house:x", "@g:x", false),
        ]);
        let now = at(2026, 10, 22, 10, 0);
        sync(&e, house_read(vec![f.clone(), g.clone()], now), now).await;
        assert_eq!(admin_notes(&e).len(), 1);
        // active again, then deactivated again another day: told again
        let now = at(2026, 10, 23, 10, 0);
        sync(
            &e,
            house_read(vec![resident("f", &["residents"]), g.clone()], now),
            now,
        )
        .await;
        e.chat
            .members
            .lock()
            .unwrap()
            .push(in_room("!house:x", "@f:x", false));
        let now = at(2026, 10, 24, 10, 0);
        sync(&e, house_read(vec![f, g], now), now).await;
        let notes = admin_notes(&e);
        assert_eq!(notes.len(), 2, "{notes:?}");
        assert!(notes[1].contains("f is deactivated"), "{}", notes[1]);

        // the same hold again after it ended: a new notice
        let e = managing(vec![house_with_group()]);
        let everyone = ["@a:x", "@b:x", "@c:x", "@d:x", "@e:x"];
        let fill = |e: &Engine<FakeWiki, FakeChat>| {
            let mut m = e.chat.members.lock().unwrap();
            m.clear();
            m.extend(everyone.iter().map(|w| in_room("!house:x", w, false)));
        };
        let few = || vec![resident("a", &["residents"])];
        let all = || {
            ["a", "b", "c", "d", "e"]
                .iter()
                .map(|n| resident(n, &["residents"]))
                .collect::<Vec<_>>()
        };
        fill(&e);
        let now = at(2026, 10, 22, 10, 0);
        sync(&e, house_read(few(), now), now).await;
        assert_eq!(admin_notes(&e).len(), 1);
        let now = at(2026, 10, 22, 10, 15);
        sync(&e, house_read(all(), now), now).await;
        assert!(e.state.lock().await.rooms["!house:x"].hold.is_none());
        let now = at(2026, 10, 23, 10, 0);
        sync(&e, house_read(few(), now), now).await;
        let notes = admin_notes(&e);
        assert_eq!(notes.len(), 2, "{notes:?}");
        assert!(notes[1].contains("4 removals held"), "{}", notes[1]);
        assert!(e.chat.removed.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn many_removals_wait_for_an_admin() {
        let e = managing(vec![house_with_group()]);
        e.chat
            .aliases
            .lock()
            .unwrap()
            .push(("#house-plenum:x".into(), "!house:x".into()));
        for who in ["@a:x", "@b:x", "@c:x", "@d:x", "@e:x", "@f:x"] {
            e.chat
                .members
                .lock()
                .unwrap()
                .push(in_room("!house:x", who, false));
        }
        // a wrong read: only a, b and a newcomer live here; four would go at once
        let people = || {
            vec![
                resident("a", &["residents"]),
                resident("b", &["residents"]),
                resident("new", &["residents"]),
            ]
        };
        for minutes in [0, 15] {
            let now = at(2026, 10, 22, 10, minutes);
            sync(&e, house_read(people(), now), now).await;
        }
        assert!(e.chat.removed.lock().unwrap().is_empty());
        // invitations still go out
        assert!(members_of(&e, "!house:x").contains("@new:x"));
        let notes = admin_notes(&e);
        assert_eq!(notes.len(), 1, "told once: {notes:?}");
        let digest = e.state.lock().await.rooms["!house:x"]
            .hold
            .as_ref()
            .unwrap()
            .digest();
        assert!(
            notes[0].contains("4 removals held")
                && notes[0].contains(&format!("!members confirm house {digest}")),
            "{}",
            notes[0]
        );
        let admin = Sender {
            id: "@admin:x".try_into().unwrap(),
            name: "Admin".into(),
        };
        let now = at(2026, 10, 22, 10, 20);
        let dm: OwnedRoomId = "!admin-dm:x".try_into().unwrap();
        // not in a room, not for an unknown room
        let a = e
            .command(&room(), &admin, cmd("!members confirm house"), now)
            .await;
        assert!(a.contains("direct chat"), "{a}");
        let a = e
            .command_in(
                &dm,
                &admin,
                cmd("!members confirm nowhere"),
                now,
                Scope::AdminDm,
            )
            .await;
        assert!(a.contains("No room"), "{a}");
        let a = e
            .command_in(&dm, &admin, cmd("!members"), now, Scope::AdminDm)
            .await;
        assert!(
            a.contains("4 removals held") && a.contains(&format!("`{digest}`")),
            "{a}"
        );
        // without the digest, or another one: nothing is confirmed, the plan is shown
        for wrong in ["!members confirm house", "!members confirm house 00000000"] {
            let a = e
                .command_in(&dm, &admin, cmd(wrong), now, Scope::AdminDm)
                .await;
            assert!(
                a.contains("nothing was confirmed")
                    && a.contains(&digest)
                    && a.contains("@c:x, @d:x, @e:x, @f:x"),
                "{a}"
            );
            assert!(
                !e.state.lock().await.rooms["!house:x"]
                    .hold
                    .as_ref()
                    .unwrap()
                    .confirmed
            );
        }
        assert!(e.chat.removed.lock().unwrap().is_empty());
        // confirmed by its alias and digest: carried out at once with the last read
        let a = e
            .command_in(
                &dm,
                &admin,
                cmd(&format!("!members confirm #house-plenum:x {digest}")),
                now,
                Scope::AdminDm,
            )
            .await;
        assert!(
            a.contains("carried out: removed @c:x, @d:x, @e:x, @f:x."),
            "{a}"
        );
        assert_eq!(
            removed_from(&e, "!house:x"),
            ["@c:x", "@d:x", "@e:x", "@f:x"]
        );
        assert!(e.state.lock().await.rooms["!house:x"].hold.is_none());
        let a = e
            .command_in(
                &dm,
                &admin,
                cmd(&format!("!members confirm house {digest}")),
                now,
                Scope::AdminDm,
            )
            .await;
        assert!(a.contains("Nothing is held"), "{a}");
    }

    #[tokio::test]
    async fn a_confirmation_is_for_the_plan_it_saw() {
        let e = managing(vec![house_with_group()]);
        for who in ["@a:x", "@b:x", "@c:x", "@d:x", "@e:x"] {
            e.chat
                .members
                .lock()
                .unwrap()
                .push(in_room("!house:x", who, false));
        }
        let now = at(2026, 10, 22, 10, 0);
        sync(
            &e,
            house_read(vec![resident("a", &["residents"])], now),
            now,
        )
        .await;
        assert_eq!(admin_notes(&e).len(), 1);
        {
            let mut st = e.state.lock().await;
            let room = st.rooms.get_mut("!house:x").unwrap();
            let digest = room.hold.as_ref().unwrap().digest();
            assert_eq!(room.confirm(&digest), Confirm::Confirmed);
        }
        // the next read is different (b came back): a new plan, held and told anew
        let people = vec![resident("a", &["residents"]), resident("b", &["residents"])];
        let now = at(2026, 10, 22, 10, 15);
        // c, d, e: three of five is still more than two
        sync(&e, house_read(people.clone(), now), now).await;
        assert!(e.chat.removed.lock().unwrap().is_empty());
        assert_eq!(admin_notes(&e).len(), 2);
        // the plan shrinks within the limit: the hold ends by itself
        let mut more = people;
        more.push(resident("c", &["residents"]));
        let now = at(2026, 10, 22, 10, 30);
        sync(&e, house_read(more, now), now).await;
        assert_eq!(removed_from(&e, "!house:x"), ["@d:x", "@e:x"]);
        assert!(e.state.lock().await.rooms["!house:x"].hold.is_none());
    }

    #[tokio::test]
    async fn a_restart_between_rounds_repeats_nothing() {
        let mut t = setup(vec![house_with_group(), kitchen()]);
        t.engine.me = Some("@plenum-bot:x".try_into().unwrap());
        t.engine.ask_to_choose = true;
        t.engine.choose_url = Some("https://login.x/flow/".into());
        t.engine.chat.members.lock().unwrap().extend([
            in_room("!house:x", "@old:x", false),
            in_room("!k1:x", "@a:x", false),
        ]);
        let people = || {
            vec![
                resident("a", &["residents", "kitchen-floor-1"]),
                resident("b", &["residents", "kitchen-floor-1"]),
                unchosen("u", &["residents"]),
            ]
        };
        let now = at(2026, 10, 22, 10, 0);
        sync(&t.engine, house_read(people(), now), now).await;
        assert_eq!(removed_from(&t.engine, "!house:x"), ["@old:x"]);
        let members = t.engine.chat.members.lock().unwrap().clone();
        assert_eq!(t.engine.chat.dms.lock().unwrap().len(), 1);
        // each room's state was saved on its own
        let saved = State::load(&t._dir.path().join("state.json"))
            .await
            .unwrap();
        assert!(saved.rooms.contains_key("!house:x") && saved.rooms.contains_key("!k1:x"));
        restart(&mut t).await;
        t.engine.me = Some("@plenum-bot:x".try_into().unwrap());
        t.engine.ask_to_choose = true;
        t.engine.choose_url = Some("https://login.x/flow/".into());
        *t.engine.chat.members.lock().unwrap() = members.clone();
        let now = at(2026, 10, 22, 10, 15);
        sync(&t.engine, house_read(people(), now), now).await;
        assert_eq!(*t.engine.chat.members.lock().unwrap(), members);
        assert!(t.engine.chat.removed.lock().unwrap().is_empty());
        assert!(t.engine.chat.dms.lock().unwrap().is_empty());
        assert!(t.engine.chat.admin_notes.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn mentions_go_to_the_chosen_account_else_the_house_account() {
        let e = managing(vec![house_with_group()]);
        let mut gone = resident("gone", &[]);
        gone.active = false;
        e.set_people(&People::new(vec![
            linked(unchosen("ben", &[]), "@ben:matrix.org", true),
            linked(unchosen("anna", &[]), "@anna:matrix.org", false),
            gone,
        ]));
        assert_eq!(
            e.account_of("ben").map(|a| a.to_string()).as_deref(),
            Some("@ben:matrix.org")
        );
        assert_eq!(
            e.account_of("Anna").map(|a| a.to_string()).as_deref(),
            Some("@anna:x")
        );
        assert_eq!(e.account_of("gone"), None);
    }

    #[tokio::test]
    async fn every_area_has_an_example_and_untouched_pages_get_the_newest_text() {
        let t = setup(vec![plenum("house", "House plenum", "!house:x")]);
        let e = &t.engine;
        e.tick(at(2026, 10, 20, 12, 0)).await;
        let example = e
            .wiki
            .by_title("House plenum: Example minutes")
            .expect("the example");
        assert_eq!(example.text, protocol::example_doc_text(&e.plenums[0]));
        // the example is linked from the overview, its decisions count nowhere
        let o = e.wiki.by_title("House plenum: Overview").unwrap();
        assert!(
            o.text
                .contains("How finished minutes look: [House plenum: Example minutes]"),
            "{}",
            o.text
        );
        assert!(e.state.lock().await.plenums["house"].decisions.is_empty());
        // an untouched page of the bot's with an older text: renewed at the next check
        {
            let mut docs = e.wiki.docs.lock().unwrap();
            let (d, _) = docs
                .iter_mut()
                .find(|(d, _)| d.title == "House plenum: Minutes")
                .unwrap();
            d.text = "an older text".into();
        }
        // ... one someone wrote in: left alone
        edit_doc(e, "House plenum: Rules", |t| t.push_str("\nOur rule."));
        e.tick(at(2026, 10, 20, 13, 1)).await;
        let minutes = e.wiki.by_title("House plenum: Minutes").unwrap();
        assert_eq!(minutes.text, protocol::protocols_doc_text(&e.plenums[0]));
        assert!(e
            .wiki
            .by_title("House plenum: Rules")
            .unwrap()
            .text
            .ends_with("Our rule."));
    }

    #[tokio::test]
    async fn a_private_plenum_shows_only_in_its_room_and_to_admins() {
        let t = setup(vec![plenum("house", "House plenum", "!house:x"), finance()]);
        let e = &t.engine;
        let elsewhere: OwnedRoomId = "!botroom:x".try_into().unwrap();
        let now = at(2026, 10, 20, 12, 0);
        let a = e.command(&elsewhere, &anna(), cmd("!plenum"), now).await;
        assert!(
            a.contains("House plenum") && !a.contains("Finance plenum"),
            "{a}"
        );
        let fin: OwnedRoomId = "!fin:x".try_into().unwrap();
        let a = e.command(&fin, &anna(), cmd("!plenum"), now).await;
        assert!(a.contains("Finance plenum"), "{a}");
        let a = e
            .command_in(&elsewhere, &anna(), cmd("!plenum"), now, Scope::AdminDm)
            .await;
        assert!(
            a.contains("Finance plenum") && a.contains("House plenum"),
            "{a}"
        );
    }

    // ── What of the minutes goes into the chat ───────────────────────────────────

    fn person(id: &str, name: &str) -> Sender {
        Sender {
            id: id.try_into().unwrap(),
            name: name.into(),
        }
    }

    async fn say(
        e: &Engine<FakeWiki, FakeChat>,
        who: &Sender,
        s: &str,
        now: DateTime<Utc>,
    ) -> String {
        e.command(&room(), who, cmd(s), now).await
    }

    /// A restart: a new engine from the state file, with the same wiki and a new chat.
    async fn restart(t: &mut T) {
        let path = t._dir.path().join("state.json");
        let state = State::load(&path).await.unwrap();
        let old = &mut t.engine;
        let wiki = std::mem::take(&mut old.wiki);
        let (plenums, tz, pdf, admins) = (
            old.plenums.clone(),
            old.tz,
            old.pdf.clone(),
            old.admins.clone(),
        );
        t.engine = Engine::new(
            plenums,
            tz,
            wiki,
            FakeChat::default(),
            pdf,
            admins,
            state,
            path,
        );
    }

    #[tokio::test]
    async fn a_confidential_meeting_keeps_its_minutes_in_the_wiki_and_out_of_the_chat() {
        let t = setup(vec![plenum("house", "House plenum", "!house:x")]);
        let e = &t.engine;
        let house: OwnedUserId = "@ben:house.org".try_into().unwrap();
        e.set_people(&People::new(vec![DirPerson {
            id: "u-ben".into(),
            username: "ben".into(),
            name: "Ben Berg".into(),
            active: true,
            accounts: vec![house.clone()],
            groups: Default::default(),
            house: Some(house),
            links: Vec::new(),
            choice: Choice::None,
            stay_until: None,
        }]));
        let now = at(2026, 10, 20, 12, 0);
        let a = e
            .command(&room(), &anna(), cmd("!minutes confidential"), now)
            .await;
        assert!(
            a.starts_with("🔒 House plenum on Sunday, 1 November 2026 at 19:00"),
            "the next meeting: {a}"
        );
        e.command(&room(), &anna(), cmd("!top Müll"), now).await;

        e.tick(at(2026, 10, 29, 19, 0)).await; // prepared and announced
        let doc = e.wiki.by_title("House plenum 2026-11-01").expect("made");
        assert!(doc.text.contains("### Müll"), "chat items still go in");
        let a = e
            .command(&room(), &anna(), cmd("!top Kaffee"), at(2026, 10, 30, 9, 0))
            .await;
        assert!(a.contains("On the page") && !a.contains("wiki.test"), "{a}");
        let a = e
            .command(&room(), &anna(), cmd("!plenum"), at(2026, 10, 30, 9, 0))
            .await;
        assert!(
            a.contains("Minutes: confidential") && !a.contains("2026-11-01"),
            "{a}"
        );
        e.tick(at(2026, 11, 1, 18, 30)).await; // the reminder
        edit_doc(e, "House plenum 2026-11-01", |t| {
            t.push_str(
                "\nDecision: Kompost kommt.\n- [ ] @[Ben Berg](mention://m1/user/u1): Tonne kaufen\n",
            );
            // both topics were dealt with (a topic carried on would make December
            // confidential too: a_topic_from_a_confidential_meeting_keeps_the_next_one_quiet)
            *t = t.replace("**Result:**", "**Result:** done");
        });
        e.tick(at(2026, 11, 2, 9, 0)).await; // the result box (in the wiki only)
        e.tick(at(2026, 11, 2, 9, 1)).await; // closed

        let texts = e.chat.texts();
        assert_eq!(texts.len(), 3, "announcement, reminder, closed: {texts:?}");
        for t in &texts {
            for secret in ["wiki.test", "Kompost", "Tonne", "Ben", "Müll", "item"] {
                assert!(!t.contains(secret), "{secret} in {t}");
            }
        }
        assert!(texts[2].contains("are closed") && texts[2].contains("confidential"));
        assert!(
            e.chat.files.lock().unwrap().is_empty(),
            "no PDF in the room"
        );
        assert!(e.chat.pinged.lock().unwrap().is_empty(), "nobody pinged");
        // The wiki got everything as always.
        let decisions = e.wiki.by_title("House plenum: Decisions").unwrap();
        assert!(
            decisions.text.contains("* Kompost kommt."),
            "{}",
            decisions.text
        );
        if e.pdf.enabled {
            assert_eq!(e.wiki.attachments.lock().unwrap().len(), 1);
        }

        // The open task comes along to December's minutes, which are shared in full: its
        // summary has December's own task, not the one from the confidential meeting.
        e.tick(at(2026, 12, 3, 19, 0)).await;
        let december = e.wiki.by_title("House plenum 2026-12-06").expect("made");
        assert!(december.text.contains("Tonne kaufen"), "{}", december.text);
        assert!(e.chat.texts()[3].contains("wiki.test/doc/house-plenum-2026-12-06"));
        edit_doc(e, "House plenum 2026-12-06", |t| {
            t.push_str("\n- [ ] Cara: Liste schreiben\n")
        });
        e.tick(at(2026, 12, 7, 9, 0)).await;
        e.tick(at(2026, 12, 7, 9, 1)).await;
        let summary = e.chat.texts().last().unwrap().clone();
        assert!(summary.contains("- Cara: Liste schreiben"), "{summary}");
        assert!(!summary.contains("Tonne"), "{summary}");
        assert!(e.chat.pinged.lock().unwrap().is_empty());
        if e.pdf.enabled {
            // December's PDF in the room leaves the task out; the wiki's has it
            let wiki = e.wiki.attachments.lock().unwrap()[1].2;
            let room = e.chat.files.lock().unwrap()[0].2;
            assert!(room < wiki, "room {room} bytes, wiki {wiki}");
        }
    }

    #[tokio::test]
    async fn link_only_shares_the_link_and_nothing_from_the_minutes() {
        let t = setup(vec![plenum("house", "House plenum", "!house:x")]);
        let e = &t.engine;
        e.tick(at(2026, 10, 29, 19, 0)).await; // announced in full, as before
        assert!(e.chat.texts()[0].contains("the page for its topics and minutes is open"));
        let a = e
            .command(
                &room(),
                &anna(),
                cmd("!minutes link"),
                at(2026, 10, 30, 9, 0),
            )
            .await;
        assert!(a.starts_with("🔗"), "{a}");
        edit_doc(e, "House plenum 2026-11-01", |t| {
            t.push_str("\nDecision: Kompost kommt.\n- [ ] Ben: Tonne kaufen\n")
        });
        e.tick(at(2026, 11, 1, 18, 30)).await;
        e.tick(at(2026, 11, 2, 9, 0)).await;
        e.tick(at(2026, 11, 2, 9, 1)).await;
        let texts = e.chat.texts();
        assert_eq!(texts.len(), 3, "{texts:?}");
        let link = "https://wiki.test/doc/house-plenum-2026-11-01";
        assert!(
            texts[1].contains(link) && texts[2].contains(link),
            "{texts:?}"
        );
        assert!(texts[2].contains("are closed"));
        assert!(!texts[2].contains("Kompost") && !texts[2].contains("Tonne"));
        assert!(
            e.chat.files.lock().unwrap().is_empty(),
            "no PDF in the room"
        );
        let st = e.state.lock().await;
        assert!(
            st.plenums["house"].meetings["house-series#2026-11-01"]
                .close
                .done
        );
        assert_eq!(st.plenums["house"].withheld_tasks, ["Ben: Tonne kaufen"]);
    }

    #[tokio::test]
    async fn a_change_reaches_a_close_that_failed_and_holds_after_a_restart() {
        let mut t = setup(vec![plenum("house", "House plenum", "!house:x")]);
        let e = &t.engine;
        e.tick(at(2026, 10, 29, 19, 0)).await;
        edit_doc(e, "House plenum 2026-11-01", |t| {
            t.push_str("\nDecision: Kompost kommt.\n")
        });
        // The close begins while Matrix is down: what is for the wiki is done, the chat fails.
        *e.chat.fail.lock().unwrap() = true;
        e.tick(at(2026, 11, 2, 9, 0)).await;
        assert!(
            !e.state.lock().await.plenums["house"].meetings["house-series#2026-11-01"]
                .close
                .done
        );
        // Before the next try someone makes it confidential: the meeting whose minutes are open.
        let a = e
            .command(
                &room(),
                &anna(),
                cmd("!minutes confidential"),
                at(2026, 11, 2, 9, 1),
            )
            .await;
        assert!(a.contains("Sunday, 1 November 2026"), "{a}");
        assert!(
            a.contains("in the chat already"),
            "the announcement had the link: {a}"
        );

        restart(&mut t).await;
        let e = &t.engine;
        e.tick(at(2026, 11, 2, 9, 2)).await;
        let texts = e.chat.texts();
        assert_eq!(texts.len(), 1, "{texts:?}");
        assert!(texts[0].contains("confidential") && !texts[0].contains("Kompost"));
        assert!(e.chat.files.lock().unwrap().is_empty());
        let decisions = e.wiki.by_title("House plenum: Decisions").unwrap();
        assert_eq!(decisions.text.matches("Kompost kommt").count(), 1);
        e.tick(at(2026, 11, 2, 9, 3)).await;
        assert_eq!(e.chat.texts().len(), 1, "closed once");
    }

    #[tokio::test]
    async fn everyone_shares_less_and_only_some_share_more_again() {
        let mut p = plenum("house", "House plenum", "!house:x");
        p.organizers = HashSet::from(["@orga:x".try_into().unwrap()]);
        let t = setup(vec![p]);
        let e = &t.engine;
        let now = at(2026, 10, 20, 12, 0);
        let (anna, ben) = (anna(), person("@ben:x", "Ben"));
        let (orga, admin) = (person("@orga:x", "Orga"), person("@admin:x", "Admin"));
        let say = |who, s| say(e, who, s, now);
        assert!(say(&anna, "!minutes confidential").await.starts_with("🔒"));
        assert!(say(&ben, "!minutes full").await.starts_with("⛔"));
        assert!(
            say(&ben, "!minutes link").await.starts_with("⛔"),
            "link shares more"
        );
        assert!(say(&anna, "!minutes confidential")
            .await
            .contains("already so"));
        // who set it takes it back
        assert!(say(&anna, "!minutes link").await.starts_with("🔗"));
        assert!(say(&ben, "!minutes confidential").await.starts_with("🔒"));
        assert!(
            say(&anna, "!minutes full").await.starts_with("⛔"),
            "Ben set it now"
        );
        assert!(say(&orga, "!minutes link").await.starts_with("🔗"));
        assert!(say(&admin, "!minutes full").await.starts_with("📋"));
        let st = e.state.lock().await;
        let m = &st.plenums["house"].meetings["house-series#2026-11-01"];
        assert_eq!(m.publication, Some(Publication::Full));
        assert_eq!(m.publication_by.as_deref(), Some("@admin:x"));
        assert!(!m.created, "a setting makes no protocol early");
    }

    #[tokio::test]
    async fn a_plenum_can_be_confidential_by_default() {
        let mut p = plenum("finance", "Finance plenum", "!house:x");
        p.publication = Publication::Confidential;
        let t = setup(vec![p]);
        let e = &t.engine;
        e.tick(at(2026, 10, 29, 19, 0)).await;
        let texts = e.chat.texts();
        assert!(
            texts[0].starts_with("🔒") && !texts[0].contains("wiki.test"),
            "{texts:?}"
        );
        // sharing more than the config says is for the organizers and the admins
        let a = e
            .command(
                &room(),
                &anna(),
                cmd("!minutes link"),
                at(2026, 10, 30, 9, 0),
            )
            .await;
        assert!(a.starts_with("⛔"), "{a}");
        let admin = person("@admin:x", "Admin");
        let a = e
            .command(
                &room(),
                &admin,
                cmd("!minutes link"),
                at(2026, 10, 30, 9, 0),
            )
            .await;
        assert!(a.starts_with("🔗"), "{a}");
        e.tick(at(2026, 11, 1, 18, 30)).await;
        assert!(e.chat.texts()[1].contains("wiki.test/doc/finance-plenum-2026-11-01"));
    }

    #[tokio::test]
    async fn a_day_picks_the_meeting_and_the_setting_follows_it_through_the_calendar() {
        let t = setup(vec![plenum("house", "House plenum", "!house:x")]);
        let e = &t.engine;
        let now = at(2026, 10, 20, 12, 0);
        let anna = anna();
        let say = |s, now| say(e, &anna, s, now);
        calendar(
            e,
            "house",
            |l| l.push(occ("next-year", "2027-12-06 19:00")),
            now,
        )
        .await;
        let a = say("!minutes confidential 6.12.", now).await;
        assert!(
            a.contains("Sunday, 6 December 2026"),
            "the next 6 December: {a}"
        );
        let a = say("!minutes confidential 7.12.", now).await;
        assert!(a.contains("no open meeting on 7 Dec"), "{a}");
        calendar(
            e,
            "house",
            |l| l.push(occ("extra", "2026-12-06 10:00")),
            now,
        )
        .await;
        let a = say("!minutes link 6.12.", now).await;
        assert!(
            a.contains("several meetings") && a.contains("10:00, 19:00"),
            "{a}"
        );
        let a = say("!minutes link 6.12. 10:00", now).await;
        assert!(a.starts_with("🔗") && a.contains("10:00"), "{a}");
        let a = say("!plenum", now).await;
        assert!(
            a.contains("In the chat: the link, and after"),
            "November in full: {a}"
        );

        // November is shared as always.
        e.tick(at(2026, 10, 29, 19, 0)).await;
        e.tick(at(2026, 11, 2, 9, 0)).await;
        e.tick(at(2026, 11, 2, 9, 1)).await;
        assert!(e.chat.texts()[0].contains("wiki.test"));
        let a = say("!minutes confidential 1.11.", at(2026, 11, 2, 10, 0)).await;
        assert!(a.contains("closed already"), "{a}");

        // December's meeting moves a day in the calendar: it keeps its setting.
        calendar(
            e,
            "house",
            |l| {
                for o in l.iter_mut().filter(|o| o.id == "house-series#2026-12-06") {
                    *o = occ(&o.id.clone(), "2026-12-07 19:00");
                }
            },
            at(2026, 11, 20, 12, 0),
        )
        .await;
        e.tick(at(2026, 12, 4, 19, 0)).await;
        // its page opens once the extra meeting on the 6th is over, or a day before
        e.tick(at(2026, 12, 6, 19, 0)).await;
        let texts = e.chat.texts();
        let december: Vec<&String> = texts
            .iter()
            .filter(|t| t.contains("7 December 2026"))
            .collect();
        assert_eq!(december.len(), 1, "{texts:?}");
        assert!(december[0].starts_with("🔒") && !december[0].contains("wiki.test"));
        let extra: Vec<&String> = texts.iter().filter(|t| t.contains("10:00")).collect();
        assert!(
            extra
                .iter()
                .all(|t| !t.contains("Decisions") && !t.contains("summary")),
            "{extra:?}"
        );
    }

    // ── Plenum pages (docs/PLENUM-PAGES.md) ─────────────────────────────────────

    fn admin() -> Sender {
        Sender {
            id: "@admin:x".try_into().unwrap(),
            name: "Admin".into(),
        }
    }

    fn quiet() -> Plenum {
        let mut p = plenum("house", "House plenum", "!house:x");
        p.messages = Messages::Quiet;
        p
    }

    /// The link message of a meeting, as the room shows it now.
    async fn link_shown(e: &Engine<FakeWiki, FakeChat>, id: &str) -> String {
        let event = e.state.lock().await.plenums["house"].meetings[id]
            .link
            .clone()
            .expect("a link message")
            .event;
        e.chat.shown(&event).unwrap()
    }

    #[tokio::test]
    async fn quiet_posts_only_the_link_and_edits_it() {
        let t = setup(vec![quiet()]);
        let e = &t.engine;
        let a = e
            .command(&room(), &anna(), cmd("!top Müll"), at(2026, 10, 20, 12, 0))
            .await;
        assert!(a.contains("Noted"), "answers still come: {a}");
        e.tick(at(2026, 10, 29, 19, 0)).await;
        assert_eq!(e.chat.kinds(), [Notice::Link]);
        // moved: the message is edited, no new one
        calendar(
            e,
            "house",
            |l| {
                for o in l.iter_mut().filter(|o| o.id == "house-series#2026-11-01") {
                    *o = occ(&o.id.clone(), "2026-11-02 19:00");
                }
            },
            at(2026, 10, 30, 9, 0),
        )
        .await;
        e.tick(at(2026, 10, 30, 9, 0)).await;
        e.tick(at(2026, 10, 30, 9, 1)).await;
        assert_eq!(e.chat.kinds(), [Notice::Link]);
        let shown = link_shown(e, "house-series#2026-11-01").await;
        assert!(
            shown.contains("Monday, 2 November") && shown.contains("moved from"),
            "{shown}"
        );
        // no reminder, nothing after the meeting, no PDF; the page has its result all the same
        for (d, h, m) in [(2, 18, 30), (2, 19, 0), (3, 9, 0), (3, 9, 1), (3, 10, 0)] {
            e.tick(at(2026, 11, d, h, m)).await;
        }
        assert_eq!(e.chat.kinds(), [Notice::Link]);
        assert!(e.chat.files.lock().unwrap().is_empty());
        let doc = e.wiki.by_title("House plenum 2026-11-02").unwrap();
        assert!(page::has_box(&doc.text), "{}", doc.text);
        assert!(
            e.state.lock().await.plenums["house"].meetings["house-series#2026-11-01"]
                .close
                .done
        );
        // the next page: one more link; its cancellation edits it
        e.tick(at(2026, 12, 3, 19, 0)).await;
        assert_eq!(e.chat.kinds(), [Notice::Link, Notice::Link]);
        calendar(
            e,
            "house",
            |l| l.retain(|o| o.id != "house-series#2026-12-06"),
            at(2026, 12, 4, 9, 0),
        )
        .await;
        e.tick(at(2026, 12, 4, 9, 0)).await;
        assert_eq!(e.chat.kinds(), [Notice::Link, Notice::Link]);
        let shown = link_shown(e, "house-series#2026-12-06").await;
        assert!(shown.contains("cancelled"), "{shown}");
        // a setting changed in an admin's direct chat: not said in a quiet room
        let dm: OwnedRoomId = "!dm:x".try_into().unwrap();
        let a = e
            .command_in(
                &dm,
                &admin(),
                crate::commands::parse_for("!plenum set house reminder 60", &["house"]).unwrap(),
                at(2026, 12, 4, 10, 0),
                Scope::AdminDm,
            )
            .await;
        assert!(a.contains("changed"), "{a}");
        assert_eq!(e.chat.kinds(), [Notice::Link, Notice::Link]);
        // switched to all: nothing stale comes now
        let a = e
            .command(
                &room(),
                &anna(),
                cmd("!plenum set messages all"),
                at(2026, 12, 4, 10, 1),
            )
            .await;
        assert!(a.contains("quiet → **all**"), "{a}");
        e.tick(at(2026, 12, 4, 10, 2)).await;
        assert_eq!(e.chat.kinds(), [Notice::Link, Notice::Link]);
        assert!(e.chat.pinged.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn all_tags_every_message_and_quiet_passes_only_the_link() {
        for messages in [Messages::All, Messages::Quiet] {
            for notice in [
                Notice::Link,
                Notice::Moved,
                Notice::Reminder,
                Notice::Cancelled,
                Notice::Summary,
                Notice::Pdf,
                Notice::Setting,
            ] {
                assert_eq!(
                    sends(messages, notice),
                    messages == Messages::All || notice == Notice::Link
                );
            }
        }
    }

    #[tokio::test]
    async fn a_topic_from_a_confidential_meeting_keeps_the_next_one_confidential() {
        let t = setup(vec![plenum("house", "House plenum", "!house:x")]);
        let e = &t.engine;
        let now = at(2026, 10, 20, 12, 0);
        e.command(&room(), &anna(), cmd("!minutes confidential"), now)
            .await;
        e.command(
            &room(),
            &anna(),
            cmd("!top Mietschulden\nDetails nur hier."),
            now,
        )
        .await;
        e.tick(at(2026, 10, 29, 19, 0)).await;
        e.tick(at(2026, 11, 2, 9, 0)).await;
        e.tick(at(2026, 11, 2, 9, 1)).await;
        e.tick(at(2026, 12, 3, 19, 0)).await;
        let december = e.wiki.by_title("House plenum 2026-12-06").expect("made");
        assert!(
            december.text.contains("### Mietschulden"),
            "{}",
            december.text
        );
        let announced = e.chat.texts().last().unwrap().clone();
        assert!(
            announced.starts_with("🔒") && !announced.contains("wiki.test"),
            "{announced}"
        );
        let st = e.state.lock().await;
        assert_eq!(
            st.plenums["house"].meetings["house-series#2026-12-06"].publication,
            Some(Publication::Confidential)
        );
    }

    #[tokio::test]
    async fn a_cancelled_pages_topics_move_on_once_its_time_is_over() {
        let t = setup(vec![plenum("house", "House plenum", "!house:x")]);
        let e = &t.engine;
        e.tick(at(2026, 10, 29, 19, 0)).await;
        e.command(
            &room(),
            &anna(),
            cmd("!top Garten"),
            at(2026, 10, 29, 20, 0),
        )
        .await;
        calendar(
            e,
            "house",
            |l| l.retain(|o| o.id != "house-series#2026-11-01"),
            at(2026, 10, 30, 9, 0),
        )
        .await;
        e.tick(at(2026, 10, 30, 9, 0)).await;
        e.tick(at(2026, 10, 30, 9, 1)).await;
        assert!(e
            .wiki
            .by_title("House plenum 2026-11-01 (cancelled)")
            .is_some());
        let told = e.chat.texts().len();
        e.tick(at(2026, 11, 1, 22, 0)).await; // over
        {
            let st = e.state.lock().await;
            let ps = &st.plenums["house"];
            assert!(ps.meetings["house-series#2026-11-01"].close.done);
            assert_eq!(ps.open_topics.len(), 1);
        }
        assert_eq!(e.chat.texts().len(), told, "nothing more is said");
        e.tick(at(2026, 12, 3, 19, 0)).await;
        let december = e.wiki.by_title("House plenum 2026-12-06").expect("made");
        assert!(december.text.contains("### Garten"), "{}", december.text);
        // the cancelled page keeps its title
        assert!(e
            .wiki
            .by_title("House plenum 2026-11-01 (cancelled)")
            .is_some());
    }

    #[tokio::test]
    async fn the_next_page_opens_when_the_last_meeting_is_over() {
        let t = setup(vec![plenum("house", "House plenum", "!house:x")]);
        let e = &t.engine;
        let a = e
            .command(
                &room(),
                &anna(),
                cmd("!plenum set page_days 40"),
                at(2026, 10, 1, 9, 0),
            )
            .await;
        assert!(a.contains("3 → **40**"), "{a}");
        e.tick(at(2026, 10, 5, 9, 0)).await; // October's meeting is over: November's page
        assert!(e.wiki.by_title("House plenum 2026-11-01").is_some());
        e.tick(at(2026, 10, 30, 9, 0)).await; // December is within 40 days, but not yet
        assert!(e.wiki.by_title("House plenum 2026-12-06").is_none());
        e.tick(at(2026, 11, 2, 9, 0)).await; // November's closing time
        assert!(e.wiki.by_title("House plenum 2026-12-06").is_some());
        // a topic from the chat goes to the page that is open
        let a = e
            .command(&room(), &anna(), cmd("!top Hof"), at(2026, 11, 2, 9, 5))
            .await;
        assert!(a.contains("house-plenum-2026-12-06"), "{a}");
    }

    #[tokio::test]
    async fn a_topic_pushed_into_a_page_made_already_is_there_once() {
        let t = setup(vec![plenum("house", "House plenum", "!house:x")]);
        let e = &t.engine;
        // a meeting the next noon: its page opens a day before, while November's is open
        calendar(
            e,
            "house",
            |l| l.push(occ("extra", "2026-11-02 12:00")),
            at(2026, 10, 20, 0, 0),
        )
        .await;
        e.command(
            &room(),
            &anna(),
            cmd("!top Fahrräder"),
            at(2026, 10, 20, 12, 0),
        )
        .await;
        e.tick(at(2026, 10, 29, 19, 0)).await;
        e.tick(at(2026, 11, 1, 12, 0)).await;
        assert!(e.wiki.by_title("House plenum 2026-11-02").is_some());
        let a = e
            .command(
                &admin_room_sender(),
                &admin(),
                cmd("!minutes done"),
                at(2026, 11, 1, 21, 5),
            )
            .await;
        assert!(a.contains("is closed in a moment"), "{a}");
        for m in 6..12 {
            e.tick(at(2026, 11, 1, 21, m)).await;
        }
        let next = e.wiki.by_title("House plenum 2026-11-02").unwrap();
        assert_eq!(
            next.text.matches("### Fahrräder").count(),
            1,
            "{}",
            next.text
        );
        let titles: Vec<String> = page::topics(&next.text).iter().map(|t| t.title()).collect();
        assert_eq!(titles, ["Fahrräder"]);
        assert!(e.state.lock().await.plenums["house"].open_topics.is_empty());
    }

    fn admin_room_sender() -> OwnedRoomId {
        room()
    }

    /// The pages of the house plenum in the wiki, by title (deleted ones left out).
    fn page_titles(e: &Engine<FakeWiki, FakeChat>) -> Vec<String> {
        let mut t: Vec<String> = (e.wiki.docs.lock().unwrap().iter())
            .filter(|(d, del)| !del && d.title.starts_with("House plenum 20"))
            .map(|(d, _)| d.title.clone())
            .collect();
        t.sort();
        t
    }

    fn pinned_titles(e: &Engine<FakeWiki, FakeChat>) -> Vec<String> {
        let pins = e.wiki.pins.lock().unwrap().clone();
        let docs = e.wiki.docs.lock().unwrap();
        pins.iter()
            .filter_map(|(_, _, d)| docs.iter().find(|(doc, _)| &doc.id == d))
            .map(|(d, _)| d.title.clone())
            .collect()
    }

    fn page_after(days: u32) -> Plenum {
        let mut p = plenum("house", "House plenum", "!house:x");
        p.page_after_days = Some(days);
        p
    }

    /// October's meeting (4 Oct, 19-21) with a topic nobody reaches, closed at 21:05.
    async fn october_closed(e: &Engine<FakeWiki, FakeChat>) {
        e.tick(at(2026, 10, 2, 9, 0)).await;
        e.command(
            &room(),
            &anna(),
            cmd("!top Fahrräder"),
            at(2026, 10, 2, 10, 0),
        )
        .await;
        e.command(
            &room(),
            &admin(),
            cmd("!minutes done"),
            at(2026, 10, 4, 21, 5),
        )
        .await;
        for m in 6..12 {
            e.tick(at(2026, 10, 4, 21, m)).await;
        }
        assert!(
            e.state.lock().await.plenums["house"].meetings["house-series#2026-10-04"]
                .close
                .done
        );
    }

    #[tokio::test]
    async fn with_page_after_the_next_page_opens_the_day_after_and_only_once() {
        let mut t = setup(vec![page_after(1)]);
        october_closed(&t.engine).await;
        let e = &t.engine;
        // the minutes are closed, but the next day has not begun
        e.tick(at(2026, 10, 4, 23, 59)).await;
        assert_eq!(page_titles(e), ["House plenum 2026-10-04"]);
        e.tick(at(2026, 10, 5, 0, 0)).await;
        assert_eq!(
            page_titles(e),
            ["House plenum 2026-10-04", "House plenum 2026-11-01"],
            "November's page four weeks before, December's not"
        );
        let nov = e.wiki.by_title("House plenum 2026-11-01").unwrap();
        assert_eq!(nov.text.matches("### Fahrräder").count(), 1, "{}", nov.text);
        // the link message once, and only the open page is pinned
        let links = e
            .chat
            .kinds()
            .iter()
            .filter(|k| **k == Notice::Link)
            .count();
        assert_eq!(links, 2, "October's and November's");
        assert_eq!(pinned_titles(e), ["House plenum 2026-11-01"]);
        // restarts and ticks make nothing twice
        restart(&mut t).await;
        let e = &t.engine;
        for d in 5..20 {
            e.tick(at(2026, 10, d, 12, 0)).await;
        }
        assert_eq!(page_titles(e).len(), 2);
        assert_eq!(
            e.chat.kinds(),
            Vec::<Notice>::new(),
            "no second link message"
        );
        let nov = e.wiki.by_title("House plenum 2026-11-01").unwrap();
        assert_eq!(nov.text.matches("### Fahrräder").count(), 1);
        // a topic from the chat goes onto it at once
        let a = e
            .command(&room(), &anna(), cmd("!top Hof"), at(2026, 10, 6, 9, 0))
            .await;
        assert!(a.contains("house-plenum-2026-11-01"), "{a}");
        // it closes as usual; December's page opens the day after
        e.tick(at(2026, 11, 2, 9, 0)).await;
        for m in 1..6 {
            e.tick(at(2026, 11, 2, 9, m)).await;
        }
        assert!(e.wiki.by_title("House plenum 2026-12-06").is_some());
        assert_eq!(pinned_titles(e), ["House plenum 2026-12-06"]);
    }

    #[tokio::test]
    async fn a_late_close_holds_the_next_page_back() {
        let t = setup(vec![page_after(1)]);
        let e = &t.engine;
        e.tick(at(2026, 10, 2, 9, 0)).await;
        e.command(
            &room(),
            &anna(),
            cmd("!top Fahrräder"),
            at(2026, 10, 2, 10, 0),
        )
        .await;
        // nobody wrote !minutes done: the minutes close 12 hours after the end
        e.tick(at(2026, 10, 5, 8, 59)).await;
        assert!(e.wiki.by_title("House plenum 2026-11-01").is_none());
        for m in 0..6 {
            e.tick(at(2026, 10, 5, 9, m)).await;
        }
        let nov = e
            .wiki
            .by_title("House plenum 2026-11-01")
            .expect("opened at the close");
        assert_eq!(nov.text.matches("### Fahrräder").count(), 1, "{}", nov.text);
        assert!(e.state.lock().await.plenums["house"].open_topics.is_empty());
    }

    #[tokio::test]
    async fn page_after_0_opens_the_next_page_when_the_minutes_close() {
        let t = setup(vec![page_after(0)]);
        october_closed(&t.engine).await;
        assert!(t.engine.wiki.by_title("House plenum 2026-11-01").is_some());
    }

    #[tokio::test]
    async fn a_delay_of_days_and_the_room_setting() {
        let t = setup(vec![plenum("house", "House plenum", "!house:x")]);
        let e = &t.engine;
        let a = e
            .command(
                &room(),
                &anna(),
                cmd("!plenum set page_after 3"),
                at(2026, 10, 1, 9, 0),
            )
            .await;
        assert!(a.contains("off → **3**"), "{a}");
        let a = e
            .command(
                &room(),
                &anna(),
                cmd("!plenum settings"),
                at(2026, 10, 1, 9, 1),
            )
            .await;
        assert!(a.contains("`page_after`: **3** (set in this room)"), "{a}");
        october_closed(e).await;
        e.tick(at(2026, 10, 6, 23, 0)).await;
        assert!(e.wiki.by_title("House plenum 2026-11-01").is_none());
        e.tick(at(2026, 10, 7, 0, 0)).await;
        assert!(e.wiki.by_title("House plenum 2026-11-01").is_some());
        // off again: page_days before, as before
        let a = e
            .command(
                &room(),
                &anna(),
                cmd("!plenum set page_after off"),
                at(2026, 11, 1, 22, 0),
            )
            .await;
        assert!(a.contains("3 → **off**"), "{a}");
        for m in 0..6 {
            e.tick(at(2026, 11, 2, 9, m)).await;
        }
        e.tick(at(2026, 11, 20, 9, 0)).await;
        assert!(e.wiki.by_title("House plenum 2026-12-06").is_none());
        e.tick(at(2026, 12, 3, 19, 0)).await;
        assert!(e.wiki.by_title("House plenum 2026-12-06").is_some());
    }

    #[tokio::test]
    async fn a_page_open_for_weeks_follows_a_move_and_a_cancellation() {
        let t = setup(vec![page_after(1)]);
        let e = &t.engine;
        october_closed(e).await;
        e.tick(at(2026, 10, 5, 0, 0)).await;
        // November moves to the 8th: the same page, renamed, announced again
        calendar(
            e,
            "house",
            |l| {
                let o = l
                    .iter_mut()
                    .find(|o| o.id == "house-series#2026-11-01")
                    .unwrap();
                *o = Occurrence {
                    id: o.id.clone(),
                    ..occ("x", "2026-11-08 19:00")
                };
            },
            at(2026, 10, 10, 9, 0),
        )
        .await;
        e.tick(at(2026, 10, 10, 9, 1)).await;
        assert_eq!(
            page_titles(e),
            ["House plenum 2026-10-04", "House plenum 2026-11-08"]
        );
        let kinds = e.chat.kinds();
        assert_eq!(
            kinds.iter().filter(|k| **k == Notice::Moved).count(),
            1,
            "{kinds:?}"
        );
        // then it is cancelled: its page says so, December's opens at once (the meeting
        // before it is October's), and the cancelled page's topics move once its time is over
        calendar(
            e,
            "house",
            |l| l.retain(|o| o.id != "house-series#2026-11-01"),
            at(2026, 10, 12, 9, 0),
        )
        .await;
        e.tick(at(2026, 10, 12, 9, 1)).await;
        assert_eq!(
            page_titles(e),
            [
                "House plenum 2026-10-04",
                "House plenum 2026-11-08 (cancelled)",
                "House plenum 2026-12-06"
            ]
        );
        assert_eq!(pinned_titles(e), ["House plenum 2026-12-06"]);
        let dec = e.wiki.by_title("House plenum 2026-12-06").unwrap();
        assert_eq!(
            dec.text.matches("### Fahrräder").count(),
            0,
            "still on the cancelled page"
        );
        e.tick(at(2026, 11, 8, 21, 0)).await;
        e.tick(at(2026, 11, 8, 21, 1)).await;
        let dec = e.wiki.by_title("House plenum 2026-12-06").unwrap();
        assert_eq!(dec.text.matches("### Fahrräder").count(), 1, "{}", dec.text);
    }

    #[tokio::test]
    async fn a_series_made_anew_keeps_the_open_page() {
        let t = setup(vec![page_after(1)]);
        let e = &t.engine;
        october_closed(e).await;
        e.tick(at(2026, 10, 5, 0, 0)).await;
        // "this and all future" in Nextcloud: a new event with new IDs, from November at 20:00
        calendar(
            e,
            "house",
            |l| {
                for o in l
                    .iter_mut()
                    .filter(|o| o.start.date() > NaiveDate::from_ymd_opt(2026, 10, 31).unwrap())
                {
                    let day = o.start.date();
                    *o = occ(&format!("house-new#{day}"), &format!("{day} 20:00"));
                }
            },
            at(2026, 10, 10, 9, 0),
        )
        .await;
        for m in 1..4 {
            e.tick(at(2026, 10, 10, 9, m)).await;
        }
        assert_eq!(
            page_titles(e),
            ["House plenum 2026-10-04", "House plenum 2026-11-01"]
        );
        let st = e.state.lock().await;
        let ps = &st.plenums["house"];
        let m = &ps.meetings["house-new#2026-11-01"];
        assert!(m.created && !m.cancelled);
        assert_eq!(m.start, Some("2026-11-01 20:00".parse().unwrap()));
        assert!(!ps.meetings.contains_key("house-series#2026-11-01"));
        drop(st);
        assert!(!e.chat.kinds().contains(&Notice::Cancelled));
    }

    #[tokio::test]
    async fn a_meeting_deleted_on_a_day_with_another_is_cancelled_not_merged() {
        let t = setup(vec![page_after(1)]);
        let e = &t.engine;
        calendar(
            e,
            "house",
            |l| l.push(occ("evening", "2026-11-01 21:30")),
            at(2026, 10, 3, 9, 0),
        )
        .await;
        october_closed(e).await;
        e.tick(at(2026, 10, 5, 0, 0)).await;
        assert!(e.wiki.by_title("House plenum 2026-11-01").is_some());
        // the 19:00 meeting is deleted; the one at 21:30 stays a meeting of its own
        calendar(
            e,
            "house",
            |l| l.retain(|o| o.id != "house-series#2026-11-01"),
            at(2026, 10, 10, 9, 0),
        )
        .await;
        e.tick(at(2026, 10, 10, 9, 1)).await;
        let st = e.state.lock().await;
        let ps = &st.plenums["house"];
        assert!(ps.meetings["house-series#2026-11-01"].cancelled);
        assert!(!ps
            .meetings
            .get("evening")
            .is_some_and(|m| m.doc_id == ps.meetings["house-series#2026-11-01"].doc_id));
    }

    #[tokio::test]
    async fn an_event_entered_twice_is_one_meeting() {
        let t = setup(vec![page_after(1)]);
        let e = &t.engine;
        october_closed(e).await;
        e.tick(at(2026, 10, 5, 0, 0)).await;
        calendar(
            e,
            "house",
            |l| l.push(occ("aaa-copy", "2026-11-01 19:00")),
            at(2026, 10, 10, 9, 0),
        )
        .await;
        for d in 10..14 {
            e.tick(at(2026, 10, d, 9, 0)).await;
        }
        assert_eq!(
            page_titles(e),
            ["House plenum 2026-10-04", "House plenum 2026-11-01"]
        );
        let nov = e.wiki.by_title("House plenum 2026-11-01").unwrap();
        assert_eq!(nov.text.matches("### Fahrräder").count(), 1);
    }

    #[tokio::test]
    async fn without_the_pin_right_the_bot_tries_once_per_change() {
        let t = setup(vec![page_after(1)]);
        let e = &t.engine;
        *e.wiki.no_pins.lock().unwrap() = true;
        october_closed(e).await;
        e.tick(at(2026, 10, 5, 0, 0)).await;
        let calls = *e.wiki.pin_calls.lock().unwrap();
        for d in 5..15 {
            e.tick(at(2026, 10, d, 12, 0)).await;
        }
        assert_eq!(*e.wiki.pin_calls.lock().unwrap(), calls);
        assert!(e.wiki.by_title("House plenum 2026-11-01").is_some());
    }

    #[tokio::test]
    async fn a_pin_of_someone_else_stays() {
        let t = setup(vec![page_after(1)]);
        let e = &t.engine;
        e.tick(at(2026, 10, 2, 9, 0)).await;
        let rules = e.state.lock().await.plenums["house"]
            .rules_doc_id
            .clone()
            .unwrap();
        let col = e.state.lock().await.plenums["house"]
            .collection_id
            .clone()
            .unwrap();
        e.wiki
            .pins
            .lock()
            .unwrap()
            .push(("pin-people".into(), col, rules));
        october_closed(e).await;
        e.tick(at(2026, 10, 5, 0, 0)).await;
        let pins = e.wiki.pins.lock().unwrap().clone();
        assert!(pins.iter().any(|(p, _, _)| p == "pin-people"), "{pins:?}");
        assert_eq!(pins.len(), 2, "{pins:?}");
    }

    #[tokio::test]
    async fn a_closed_page_someone_pinned_stays_pinned() {
        let t = setup(vec![page_after(1)]);
        let e = &t.engine;
        october_closed(e).await;
        e.tick(at(2026, 10, 5, 0, 0)).await;
        assert_eq!(pinned_titles(e), ["House plenum 2026-11-01"]);
        // someone pins October's minutes
        let oct = e.wiki.by_title("House plenum 2026-10-04").unwrap();
        let col = e.state.lock().await.plenums["house"]
            .collection_id
            .clone()
            .unwrap();
        e.wiki
            .pins
            .lock()
            .unwrap()
            .push(("pin-people".into(), col, oct.id));
        for m in 0..6 {
            e.tick(at(2026, 11, 2, 9, m)).await;
        }
        assert_eq!(
            pinned_titles(e),
            ["House plenum 2026-10-04", "House plenum 2026-12-06"],
            "November's unpinned by the bot, October's kept"
        );
    }

    #[tokio::test]
    async fn a_copy_of_the_event_keeps_the_meetings_own_publication() {
        let t = setup(vec![page_after(1)]);
        let e = &t.engine;
        october_closed(e).await;
        let a = e
            .command(
                &room(),
                &anna(),
                cmd("!minutes confidential 1.11."),
                at(2026, 10, 4, 22, 0),
            )
            .await;
        assert!(a.starts_with("🔒"), "{a}");
        calendar(
            e,
            "house",
            |l| l.push(occ("aaa-copy", "2026-11-01 19:00")),
            at(2026, 10, 4, 22, 30),
        )
        .await;
        e.tick(at(2026, 10, 5, 0, 0)).await;
        assert_eq!(
            page_titles(e),
            ["House plenum 2026-10-04", "House plenum 2026-11-01"]
        );
        let last = e.chat.sent.lock().unwrap().last().unwrap().1.clone();
        assert!(last.starts_with("🔒"), "{last}");
    }

    #[test]
    fn the_day_after_is_a_local_day() {
        let t = setup(vec![page_after(1)]);
        let e = &t.engine;
        let p = &e.plenums[0];
        // the night the clocks go back (25 Oct 2026) and forward (28 Mar 2027)
        for (day, next) in [
            ("2026-10-24 19:00", (2026, 10, 25)),
            ("2027-03-27 19:00", (2027, 3, 28)),
        ] {
            let m = Meeting {
                start: Some(day.parse().unwrap()),
                ..Default::default()
            };
            assert_eq!(
                e.days_after(p, &m, 1),
                at(next.0, next.1, next.2, 0, 0),
                "{day}"
            );
        }
    }

    #[tokio::test]
    async fn the_result_box_is_written_once_and_given_up_when_it_does_not_stay() {
        let t = setup(vec![plenum("house", "House plenum", "!house:x")]);
        let e = &t.engine;
        e.tick(at(2026, 10, 29, 19, 0)).await;
        e.tick(at(2026, 11, 2, 9, 0)).await; // written
        *e.chat.fail.lock().unwrap() = true; // the room fails: the close repeats
        for m in 1..4 {
            e.tick(at(2026, 11, 2, 9, m)).await;
        }
        let doc = e.wiki.by_title("House plenum 2026-11-01").unwrap();
        assert_eq!(doc.text.matches(page::BOX_TITLE).count(), 1, "{}", doc.text);
        *e.chat.fail.lock().unwrap() = false;
        e.tick(at(2026, 11, 2, 9, 5)).await;
        assert!(e.chat.texts().last().unwrap().contains("are closed"));

        // in December the box is lost every time (a race with a browser): given up
        e.tick(at(2026, 12, 3, 19, 0)).await;
        for m in 0..=BOX_ATTEMPTS {
            e.tick(at(2026, 12, 7, 9, m)).await;
            edit_doc(e, "House plenum 2026-12-06", |t| {
                *t = page::without_box(t).replace("\n\n\n", "\n")
            });
        }
        e.tick(at(2026, 12, 7, 9, 30)).await;
        assert!(e.chat.texts().last().unwrap().contains("6 December 2026"));
        assert!(
            e.state.lock().await.plenums["house"].meetings["house-series#2026-12-06"]
                .close
                .done
        );
    }

    #[tokio::test]
    async fn room_settings_follow_the_organizers_and_the_house() {
        let mut p = plenum("house", "House plenum", "!house:x");
        p.organizers = HashSet::from(["@cara:x".try_into().unwrap()]);
        let t = setup(vec![p]);
        let e = &t.engine;
        let now = at(2026, 10, 20, 12, 0);
        let a = e
            .command(&room(), &anna(), cmd("!plenum set messages quiet"), now)
            .await;
        assert!(a.contains("Only the plenum's organizers"), "{a}");
        let cara = Sender {
            id: "@cara:x".try_into().unwrap(),
            name: "Cara".into(),
        };
        let a = e
            .command(&room(), &cara, cmd("!plenum set messages quiet"), now)
            .await;
        assert!(
            a.contains("Cara changed `messages` of House plenum: all → **quiet**"),
            "{a}"
        );
        let a = e
            .command(&room(), &anna(), cmd("!plenum settings"), now)
            .await;
        assert!(
            a.contains("`messages`: **quiet** (set in this room)"),
            "{a}"
        );
        assert!(a.contains("Last change: Cara"), "{a}");
        let a = e
            .command(&room(), &cara, cmd("!plenum set messages quiet"), now)
            .await;
        assert!(a.contains("already"), "{a}");
        let a = e
            .command(&room(), &cara, cmd("!plenum set page_days 100"), now)
            .await;
        assert!(a.contains("1 to 60"), "{a}");
        let a = e
            .command(&room(), &cara, cmd("!plenum reset messages"), now)
            .await;
        assert!(a.contains("(the default again)"), "{a}");
        // the settings are kept in a file of their own
        let saved = std::fs::read_to_string(t._dir.path().join("settings.json")).unwrap();
        assert!(saved.contains("\"by\": \"Cara\""), "{saved}");
        assert!(!std::fs::read_to_string(t._dir.path().join("state.json"))
            .unwrap()
            .contains("Cara"));

        // fixed by the house: the room cannot change them
        let mut fixed = plenum("house", "House plenum", "!house:x");
        fixed.room_settings = false;
        let t = setup(vec![fixed]);
        let a = t
            .engine
            .command(&room(), &anna(), cmd("!plenum set reminder 60"), now)
            .await;
        assert!(a.contains("set centrally"), "{a}");
    }

    #[tokio::test]
    async fn a_setting_from_an_admin_is_said_in_a_room_that_hears_everything() {
        let t = setup(vec![plenum("house", "House plenum", "!house:x")]);
        let e = &t.engine;
        let dm: OwnedRoomId = "!dm:x".try_into().unwrap();
        let a = e
            .command_in(
                &dm,
                &admin(),
                crate::commands::parse_for("!plenum set house reminder 60", &["house"]).unwrap(),
                at(2026, 10, 20, 12, 0),
                Scope::AdminDm,
            )
            .await;
        assert!(a.contains("30 → **60**"), "{a}");
        assert_eq!(e.chat.kinds(), [Notice::Setting]);
        // and the reminder comes an hour before
        e.tick(at(2026, 10, 29, 19, 0)).await;
        e.tick(at(2026, 11, 1, 18, 0)).await;
        assert_eq!(
            e.chat.kinds(),
            [Notice::Setting, Notice::Link, Notice::Reminder]
        );
    }
}
