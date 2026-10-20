//! What happens when: every minute [`Engine::tick`] walks the plenums and their meetings,
//! and the chat commands land here too. Outline and Matrix are behind [`Wiki`] and [`Chat`],
//! so the whole flow is tested with fakes.
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
//! The bot never rewrites a protocol people write in: chat items go in with Outline's
//! `patch` (only the marker line is replaced), everything else is appended. A full rewrite
//! would go through Outline's Markdown, which does not survive every text (a literal `[x]`
//! comes back as a checkbox).

use std::{collections::HashSet, path::PathBuf};

use anyhow::Result;
use chrono::{DateTime, Duration, NaiveDate, Utc};
use chrono_tz::Tz;
use mxbot_common::matrix_sdk::ruma::{OwnedRoomId, OwnedUserId, RoomId, UserId};
use tokio::sync::Mutex;
use tracing::{error, info, warn};

use crate::{
    commands::Command,
    config::{PdfConfig, Plenum},
    protocol::{self, PdfNote},
    schedule::{self, MeetingKey},
    state::{Meeting, State, Top},
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
}

/// The wiki (Outline in production).
#[allow(async_fn_in_trait)]
pub trait Wiki {
    /// The collection's ID; created if no collection has that name.
    async fn ensure_collection(&self, name: &str) -> Result<String>;
    /// A live document with exactly this title in the collection.
    async fn find_doc(&self, collection_id: &str, title: &str) -> Result<Option<Doc>>;
    async fn create_doc(
        &self,
        id: &str,
        collection_id: &str,
        title: &str,
        text: &str,
    ) -> Result<Doc>;
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
}

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
    state: Mutex<State>,
    state_path: PathBuf,
}

/// Who sent a command.
pub struct Sender {
    pub id: OwnedUserId,
    pub name: String,
}

/// The meeting `key` of plenum `p` in the state (it must exist).
fn meeting<'a>(st: &'a mut State, p: &Plenum, key: MeetingKey) -> &'a mut Meeting {
    st.plenums
        .get_mut(&p.key)
        .and_then(|ps| ps.meetings.get_mut(&key))
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
            self.tick_plenum(&mut st, p, now).await;
            let today = self.today(now);
            st.plenums.entry(p.key.clone()).or_default().prune(today);
        }
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
        let mut keys = schedule::planned(p.recurrence.as_ref(), &ps.extra, &ps.cancelled, from, to);
        // Meetings with a protocol stay in view until they are closed, even if a rule moved.
        let open: Vec<MeetingKey> = ps
            .meetings
            .iter()
            .filter(|(k, m)| m.created && !m.close.done && !m.skipped && !ps.cancelled.contains(k))
            .map(|(k, _)| *k)
            .collect();
        for k in &open {
            if !keys.contains(k) {
                keys.push(*k);
            }
        }
        // One protocol per plenum and day (its title is the date): a rule moved to another
        // time, or an extra date on a meeting day, does not get a second one.
        keys.retain(|k| {
            let taken = open.iter().any(|o| o != k && o.date() == k.date());
            let mine = ps.meetings.get(k).is_some_and(|m| m.created);
            if taken && !mine {
                warn!(plenum = %p.key, meeting = %k, "Another meeting that day has the protocol; not prepared");
            }
            mine || !taken
        });
        keys.sort();
        for key in keys {
            if let Err(e) = self.tick_meeting(st, p, key, now).await {
                warn!(plenum = %p.key, meeting = %key, "{e:#}");
            }
        }
    }

    fn end(&self, p: &Plenum, key: MeetingKey) -> DateTime<Utc> {
        key.start(self.tz) + Duration::minutes(i64::from(p.duration_minutes))
    }

    async fn tick_meeting(
        &self,
        st: &mut State,
        p: &Plenum,
        key: MeetingKey,
        now: DateTime<Utc>,
    ) -> Result<()> {
        let start = key.start(self.tz);
        let end = self.end(p, key);
        // In local days, so a DST change in between moves nothing by an hour.
        let prepare_at =
            MeetingKey(key.0 - Duration::days(i64::from(p.prepare_days))).start(self.tz);
        {
            let ps = st.plenums.entry(p.key.clone()).or_default();
            let m = ps.meetings.get(&key);
            if m.is_some_and(|m| m.close.done || m.skipped) {
                return Ok(());
            }
            if now < prepare_at {
                return Ok(());
            }
            if m.is_none_or(|m| !m.created) && now >= end {
                // The bot was down for the whole meeting: there is no protocol to close.
                info!(plenum = %p.key, meeting = %key, "Ended before it was prepared, skipped");
                ps.meetings.entry(key).or_default().skipped = true;
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
        let requested = st.plenums[&p.key].meetings[&key].close.requested;
        let close_at = end + Duration::hours(i64::from(p.close_after_hours));
        if requested || now >= close_at {
            self.close(st, p, key, now).await?;
        }
        Ok(())
    }

    /// The collection, the template and the decision list of a plenum, found or created
    /// again (also when someone deleted one): `(collection, template text, decisions doc)`.
    async fn setup(&self, st: &mut State, p: &Plenum) -> Result<(String, String, Doc)> {
        // Looked up every time (a protocol is prepared rarely): a cached ID of a deleted
        // collection would fail forever.
        let collection = self.wiki.ensure_collection(&p.collection).await?;
        let ps = st.plenums.entry(p.key.clone()).or_default();
        ps.collection_id = Some(collection.clone());
        let template = self
            .ensure_doc(
                &collection,
                &p.template_title(),
                protocol::DEFAULT_TEMPLATE,
                ps.template_doc_id.as_deref(),
            )
            .await?;
        ps.template_doc_id = Some(template.id.clone());
        let decisions = self
            .ensure_doc(
                &collection,
                &p.decisions_title(),
                &protocol::decisions_doc_text(p),
                ps.decisions_doc_id.as_deref(),
            )
            .await?;
        ps.decisions_doc_id = Some(decisions.id.clone());
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
            .create_doc(&uuid::Uuid::new_v4().to_string(), collection, title, text)
            .await
    }

    async fn prepare(&self, st: &mut State, p: &Plenum, key: MeetingKey) -> Result<()> {
        if st.plenums[&p.key]
            .meetings
            .get(&key)
            .is_some_and(|m| m.created)
        {
            return Ok(());
        }
        let (collection, template, _) = self.setup(st, p).await?;
        let ps = st.plenums.get_mut(&p.key).expect("set up");
        let doc_id = ps
            .meetings
            .entry(key)
            .or_insert_with(|| Meeting {
                doc_id: uuid::Uuid::new_v4().to_string(),
                ..Default::default()
            })
            .doc_id
            .clone();
        // Saved before the call: after a crash the same ID is looked up instead of a second
        // protocol being created.
        self.save(st).await;
        let title = protocol::title(p, key);
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
                let mut text = protocol::render_template(&template, p, key);
                for line in &pending {
                    text = protocol::insert_top(&text, line);
                }
                info!(plenum = %p.key, meeting = %key, "Creating the protocol");
                self.wiki
                    .create_doc(&doc_id, &collection, &title, &text)
                    .await?
            }
        };
        let ps = st.plenums.get_mut(&p.key).expect("set up");
        ps.pending_tops.clear();
        let m = ps.meetings.get_mut(&key).expect("inserted");
        m.created = true;
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

    async fn announce(&self, st: &mut State, p: &Plenum, key: MeetingKey) -> Result<()> {
        let m = meeting(st, p, key);
        if m.announce.done {
            return Ok(());
        }
        let txn = m.announce.txn();
        let link = self.link_of(m);
        let doc_id = m.doc_id.clone();
        self.save(st).await;
        // The number of items is only known in the document now; count the list lines.
        let tops = self
            .wiki
            .doc(&doc_id)
            .await?
            .map(|d| count_tops(&d.text))
            .unwrap_or(0);
        self.chat
            .send(&p.room, &protocol::announcement(p, key, &link, tops), &txn)
            .await?;
        meeting(st, p, key).announce.done = true;
        self.save(st).await;
        Ok(())
    }

    async fn remind(
        &self,
        st: &mut State,
        p: &Plenum,
        key: MeetingKey,
        started: bool,
    ) -> Result<()> {
        let m = meeting(st, p, key);
        if m.remind.done {
            return Ok(());
        }
        let txn = m.remind.txn();
        let link = self.link_of(m);
        self.save(st).await;
        self.chat
            .send(&p.room, &protocol::reminder(p, key, &link, started), &txn)
            .await?;
        meeting(st, p, key).remind.done = true;
        self.save(st).await;
        Ok(())
    }

    async fn close(
        &self,
        st: &mut State,
        p: &Plenum,
        key: MeetingKey,
        now: DateTime<Utc>,
    ) -> Result<()> {
        let (doc_id, link) = {
            let m = meeting(st, p, key);
            (m.doc_id.clone(), self.link_of(m))
        };
        let Some(doc) = self.wiki.doc(&doc_id).await? else {
            warn!(plenum = %p.key, meeting = %key, "The protocol was deleted; closed without it");
            meeting(st, p, key).close.done = true;
            self.save(st).await;
            return Ok(());
        };
        let outcome = protocol::extract(&doc.text);
        let title = protocol::title(p, key);

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
            let text = protocol::summary(p, key, &link, &outcome, decisions_link.as_deref(), note);
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
        key: MeetingKey,
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
        key: MeetingKey,
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

    /// A command from `room`; the answer for the room.
    pub async fn command(
        &self,
        room: &RoomId,
        sender: &Sender,
        cmd: Command,
        now: DateTime<Utc>,
    ) -> String {
        const ORGANIZERS_ONLY: &str = "⛔ Das dürfen hier nur die Organisator*innen.";
        let mut st = self.state.lock().await;
        let answer = match cmd {
            Command::Help => help(),
            Command::Overview(key) => self.overview(&mut st, room, key.as_deref(), now),
            Command::Top { key, text } => match self.pick(room, key.as_deref()) {
                Ok(p) => self.add_top(&mut st, p, sender, &text, now).await,
                Err(e) => e,
            },
            Command::Close(key) => match self.pick(room, key.as_deref()) {
                Ok(p) if !self.may_organize(p, &sender.id) => ORGANIZERS_ONLY.to_owned(),
                Ok(p) => self.request_close(&mut st, p, now),
                Err(e) => e,
            },
            Command::Cancel { key, date } => match self.pick(room, key.as_deref()) {
                Ok(p) if !self.may_organize(p, &sender.id) => ORGANIZERS_ONLY.to_owned(),
                Ok(p) => self.cancel(&mut st, p, date, now),
                Err(e) => e,
            },
            Command::Extra { key, date, time } => match self.pick(room, key.as_deref()) {
                Ok(p) if !self.may_organize(p, &sender.id) => ORGANIZERS_ONLY.to_owned(),
                Ok(p) => self.extra(&mut st, p, MeetingKey::new(date, time), now),
                Err(e) => e,
            },
            Command::Invalid(why) => why,
        };
        self.save(&st).await;
        drop(st);
        answer
    }

    /// The next meeting that is not closed yet, from now on (one that runs counts).
    fn next_meeting(&self, st: &mut State, p: &Plenum, now: DateTime<Utc>) -> Option<MeetingKey> {
        let today = self.today(now);
        let ps = st.plenums.entry(p.key.clone()).or_default();
        let mut keys = schedule::planned(
            p.recurrence.as_ref(),
            &ps.extra,
            &ps.cancelled,
            today - Duration::days(1),
            today + Duration::days(400),
        );
        // A protocol whose rule moved still counts until it is closed.
        keys.extend(
            ps.meetings
                .iter()
                .filter(|(k, m)| m.created && !ps.cancelled.contains(k))
                .map(|(k, _)| *k),
        );
        keys.sort();
        keys.dedup();
        keys.retain(|k| {
            self.end(p, *k) > now
                && !ps
                    .meetings
                    .get(k)
                    .is_some_and(|m| m.close.done || m.skipped)
        });
        keys.first().copied()
    }

    /// The meeting `!protokoll fertig` means: the latest one that has started and is open.
    fn current_meeting(&self, st: &State, p: &Plenum, now: DateTime<Utc>) -> Option<MeetingKey> {
        let ps = st.plenums.get(&p.key)?;
        ps.meetings
            .iter()
            .filter(|(k, m)| m.created && !m.close.done && !m.skipped && k.start(self.tz) <= now)
            .map(|(k, _)| *k)
            .max()
    }

    fn overview(
        &self,
        st: &mut State,
        room: &RoomId,
        key: Option<&str>,
        now: DateTime<Utc>,
    ) -> String {
        let list: Vec<&Plenum> = match key {
            Some(k) => self.plenums.iter().filter(|p| p.key == k).collect(),
            None => {
                let here = self.plenums_in(room);
                if here.is_empty() {
                    self.plenums.iter().collect()
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
            if let Some(r) = &p.recurrence {
                line.push_str(&format!(": {}", r.describe()));
            }
            match self.next_meeting(st, p, now) {
                Some(k) => {
                    line.push_str(&format!(
                        "\nNächstes Mal: {}",
                        schedule::describe_meeting(k)
                    ));
                    if let Some(m) = st.plenums[&p.key].meetings.get(&k).filter(|m| m.created) {
                        line.push_str(&format!("\nProtokoll: {}", self.link_of(m)));
                    }
                }
                None => line.push_str("\nKein Termin geplant."),
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
        let target = self.next_meeting(st, p, now).and_then(|k| {
            st.plenums[&p.key]
                .meetings
                .get(&k)
                .filter(|m| m.created)
                .map(|m| (k, m.doc_id.clone(), self.link_of(m)))
        });
        let Some((key, doc_id, link)) = target else {
            st.plenums
                .get_mut(&p.key)
                .expect("next_meeting made it")
                .pending_tops
                .push(top);
            return match self.next_meeting(st, p, now) {
                Some(k) => format!(
                    "📝 Notiert für das {} am {}. Der Punkt kommt ins Protokoll, sobald es angelegt ist.",
                    p.name,
                    schedule::describe_meeting(k)
                ),
                None => format!("📝 Notiert. Für das {} ist noch kein Termin geplant; der Punkt kommt ins nächste Protokoll.", p.name),
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
        meeting(st, p, key).close.requested = true;
        format!(
            "👍 Das Protokoll vom {} wird gleich abgeschlossen: Beschlüsse in die Liste, PDF, Zusammenfassung.",
            schedule::describe_meeting(key)
        )
    }

    fn cancel(&self, st: &mut State, p: &Plenum, date: NaiveDate, now: DateTime<Utc>) -> String {
        let ps = st.plenums.entry(p.key.clone()).or_default();
        let mut on_day =
            schedule::planned(p.recurrence.as_ref(), &ps.extra, &ps.cancelled, date, date);
        // A protocol whose rule moved away from this day still counts.
        for (k, m) in &ps.meetings {
            if k.date() == date && m.created && !m.close.done && !m.skipped && !on_day.contains(k) {
                on_day.push(*k);
            }
        }
        if on_day.is_empty() {
            return format!(
                "Am {} ist kein {} geplant.",
                date.format("%d.%m.%Y"),
                p.name
            );
        }
        let mut out = Vec::new();
        for key in on_day {
            if self.end(p, key) <= now {
                out.push(format!(
                    "Das {} am {} ist schon vorbei.",
                    p.name,
                    schedule::describe_meeting(key)
                ));
                continue;
            }
            if !ps.extra.remove(&key) {
                ps.cancelled.insert(key);
            }
            let note = match ps.meetings.get(&key).filter(|m| m.created) {
                Some(m) => format!(
                    " Das Protokoll ({}) bleibt im Wiki; Punkte darin bitte selbst übertragen.",
                    self.link_of(m)
                ),
                None => String::new(),
            };
            if let Some(m) = ps.meetings.get_mut(&key) {
                m.skipped = true;
            }
            out.push(format!(
                "❌ Abgesagt: {} am {}.{note}",
                p.name,
                schedule::describe_meeting(key)
            ));
        }
        out.join("\n")
    }

    fn extra(&self, st: &mut State, p: &Plenum, key: MeetingKey, now: DateTime<Utc>) -> String {
        if self.end(p, key) <= now {
            return "Dieser Termin liegt in der Vergangenheit.".to_owned();
        }
        let ps = st.plenums.entry(p.key.clone()).or_default();
        if ps.meetings.get(&key).is_some_and(|m| m.close.done) {
            return "Für diesen Termin gab es schon ein Plenum; bitte eine andere Uhrzeit nehmen."
                .to_owned();
        }
        let day = key.date();
        let mut same_day =
            schedule::planned(p.recurrence.as_ref(), &ps.extra, &ps.cancelled, day, day);
        same_day.extend(
            ps.meetings
                .iter()
                .filter(|(k, m)| k.date() == day && m.created && !m.close.done && !m.skipped)
                .map(|(k, _)| *k),
        );
        if same_day.contains(&key) {
            return format!(
                "Am {} ist das {} schon geplant.",
                schedule::describe_meeting(key),
                p.name
            );
        }
        if let Some(other) = same_day.first() {
            // One protocol per day (its title is the date).
            return format!(
                "Am {} ist schon ein {} um {} geplant. Zum Verschieben erst `!plenum absagen {}`, dann den neuen Termin.",
                day.format("%d.%m.%Y"),
                p.name,
                other.0.format("%H:%M"),
                day.format("%d.%m.")
            );
        }
        // A cancelled date comes back (with its protocol, if it had one).
        if !ps.cancelled.remove(&key) {
            ps.extra.insert(key);
        }
        if let Some(m) = ps.meetings.get_mut(&key) {
            m.skipped = false;
        }
        format!(
            "📅 {} am {} eingetragen. Das Protokoll kommt {} Tag(e) vorher.",
            p.name,
            schedule::describe_meeting(key),
            p.prepare_days
        )
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
     `!plenum absagen <Datum>`: einen Termin absagen, z. B. `!plenum absagen 27.10.`\n\
     `!plenum extra <Datum> <Uhrzeit>`: einen zusätzlichen Termin, z. B. `!plenum extra 30.10. 19:00`\n\n\
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
        async fn ensure_collection(&self, name: &str) -> Result<String> {
            self.check()?;
            let mut c = self.collections.lock().unwrap();
            if let Some((id, _)) = c.iter().find(|(_, n)| n == name) {
                return Ok(id.clone());
            }
            let id = format!("col-{}", c.len());
            c.push((id.clone(), name.to_owned()));
            Ok(id)
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
    }
}

#[cfg(test)]
mod tests {
    use super::fakes::*;
    use super::*;
    use crate::schedule::Recurrence;
    use chrono::TimeZone;

    fn plenum(key: &str, name: &str, room: &str) -> Plenum {
        Plenum {
            key: key.into(),
            name: name.into(),
            room: room.try_into().unwrap(),
            collection: "Plena".into(),
            recurrence: Some(
                toml::from_str::<Recurrence>(
                    "kind = 'monthly_nth'\nweekday = 'sun'\nnths = [1]\ntime = '19:00'",
                )
                .unwrap(),
            ),
            prepare_days: 3,
            remind_minutes: 30,
            duration_minutes: 120,
            close_after_hours: 12,
            organizers: HashSet::new(),
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
        T { engine, _dir: dir }
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
        assert!(st.plenums["haus"].meetings[&"2026-12-06 19:00".parse().unwrap()].skipped);
    }

    #[tokio::test]
    async fn dates_can_be_cancelled_and_added_by_organizers() {
        let mut p = plenum("haus", "Hausplenum", "!haus:x");
        p.organizers = HashSet::from(["@ben:x".try_into().unwrap()]);
        let t = setup(vec![p]);
        let e = &t.engine;
        let now = at(2026, 10, 20, 12, 0);
        let a = e
            .command(&room(), &anna(), cmd("!plenum absagen 1.11."), now)
            .await;
        assert!(a.starts_with("⛔"), "{a}");
        let ben = Sender {
            id: "@ben:x".try_into().unwrap(),
            name: "Ben".into(),
        };
        let a = e
            .command(&room(), &ben, cmd("!plenum absagen 1.11."), now)
            .await;
        assert!(a.contains("Abgesagt"), "{a}");
        let a = e
            .command(&room(), &ben, cmd("!plenum absagen 2.11."), now)
            .await;
        assert!(a.contains("kein Hausplenum geplant"), "{a}");
        let a = e
            .command(&room(), &ben, cmd("!plenum extra 4.11. 20:00"), now)
            .await;
        assert!(a.contains("eingetragen"), "{a}");
        let admin = Sender {
            id: "@admin:x".try_into().unwrap(),
            name: "Admin".into(),
        };
        let a = e
            .command(&room(), &admin, cmd("!plenum extra 4.11. 20:00"), now)
            .await;
        assert!(a.contains("schon geplant"), "{a}");
        let a = e.command(&room(), &anna(), cmd("!plenum"), now).await;
        assert!(
            a.contains("Nächstes Mal: Mittwoch, 04.11.2026 um 20:00"),
            "{a}"
        );

        e.tick(at(2026, 10, 29, 19, 0)).await; // the cancelled 1st gets no protocol
        assert!(e.wiki.by_title("Hausplenum 2026-11-01").is_none());
        e.tick(at(2026, 11, 1, 20, 0)).await;
        assert!(e.wiki.by_title("Hausplenum 2026-11-04").is_some());
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
        p.recurrence = Some(
            toml::from_str::<Recurrence>("kind = 'weekly'\nweekday = 'sun'\ntime = '19:00'")
                .unwrap(),
        );
        p.prepare_days = 8;
        let t = setup(vec![p]);
        let e = &t.engine;
        e.tick(at(2026, 10, 29, 19, 0)).await; // Nov 1 and Nov 8 prepared
        delete_doc(e, "Hausplenum 2026-11-01");
        // Nov 1 closes without a protocol; Nov 8 is unaffected.
        e.tick(at(2026, 11, 2, 9, 0)).await;
        assert!(e.wiki.by_title("Hausplenum 2026-11-08").is_some());
        let st = e.state.lock().await;
        assert!(
            st.plenums["haus"].meetings[&"2026-11-01 19:00".parse().unwrap()]
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
    async fn moving_the_rule_makes_no_second_protocol_that_day() {
        let t = setup(vec![plenum("haus", "Hausplenum", "!haus:x")]);
        let e = &t.engine;
        e.tick(at(2026, 10, 29, 19, 0)).await;
        // An extra date on a meeting day is refused …
        let a = e
            .command(
                &room(),
                &anna(),
                cmd("!plenum extra 1.11. 20:00"),
                at(2026, 10, 29, 19, 1),
            )
            .await;
        assert!(a.contains("schon ein Hausplenum um 19:00"), "{a}");
        // … and a rule moved to 20:00 keeps the protocol made for 19:00.
        let mut moved = e.plenums.clone();
        moved[0].recurrence = Some(
            toml::from_str::<Recurrence>(
                "kind = 'monthly_nth'\nweekday = 'sun'\nnths = [1]\ntime = '20:00'",
            )
            .unwrap(),
        );
        let state = std::mem::take(&mut *e.state.lock().await);
        let e2 = Engine::new(
            moved,
            e.tz,
            FakeWiki::default(),
            FakeChat::default(),
            e.pdf.clone(),
            e.admins.clone(),
            state,
            t._dir.path().join("state2.json"),
        );
        // Move the documents over (same wiki).
        *e2.wiki.docs.lock().unwrap() = e.wiki.docs.lock().unwrap().clone();
        *e2.wiki.collections.lock().unwrap() = e.wiki.collections.lock().unwrap().clone();
        e2.tick(at(2026, 10, 30, 12, 0)).await;
        let protocols = e2
            .wiki
            .docs
            .lock()
            .unwrap()
            .iter()
            .filter(|(d, _)| d.title == "Hausplenum 2026-11-01")
            .count();
        assert_eq!(protocols, 1);
    }

    #[tokio::test]
    async fn organizers_close_and_a_cancelled_extra_date_can_come_back() {
        let mut p = plenum("haus", "Hausplenum", "!haus:x");
        p.recurrence = None;
        p.organizers = HashSet::from(["@ben:x".try_into().unwrap()]);
        let t = setup(vec![p]);
        let e = &t.engine;
        let ben = Sender {
            id: "@ben:x".try_into().unwrap(),
            name: "Ben".into(),
        };
        let now = at(2026, 10, 20, 12, 0);
        assert!(e
            .command(&room(), &ben, cmd("!plenum extra 22.10. 19:00"), now)
            .await
            .contains("eingetragen"));
        e.tick(at(2026, 10, 20, 12, 1)).await; // prepared (3 days ahead)
        assert!(e.wiki.by_title("Hausplenum 2026-10-22").is_some());
        assert!(e
            .command(&room(), &ben, cmd("!plenum absagen 22.10."), now)
            .await
            .contains("Abgesagt"));
        let a = e
            .command(&room(), &ben, cmd("!plenum extra 22.10. 19:00"), now)
            .await;
        assert!(a.contains("eingetragen"), "{a}");
        let a = e
            .command(
                &room(),
                &anna(),
                cmd("!protokoll fertig"),
                at(2026, 10, 22, 20, 0),
            )
            .await;
        assert!(a.starts_with("⛔"), "{a}");
        let a = e
            .command(
                &room(),
                &ben,
                cmd("!protokoll fertig"),
                at(2026, 10, 22, 20, 0),
            )
            .await;
        assert!(a.contains("wird gleich"), "{a}");
        // The protocol made before the cancellation is used again, not a second one.
        e.tick(at(2026, 10, 22, 20, 1)).await;
        assert_eq!(
            e.wiki
                .docs
                .lock()
                .unwrap()
                .iter()
                .filter(|(d, _)| d.title == "Hausplenum 2026-10-22")
                .count(),
            1
        );
        assert!(e.chat.texts().last().unwrap().contains("ist abgeschlossen"));
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
}
