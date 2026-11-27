//! Minutes from the chat (docs/CHAT-MINUTES.md): a message that is minutes is taken, copied
//! into a document of its own below the meeting's page, followed when it is edited or deleted,
//! and read for decisions, tasks and postponed topics. The engine's steps for it, in an `impl`
//! of their own; `engine.rs` calls them from its tick, its close and its commands.
//!
//! As everywhere in the bot, every step saves before its side effect: the copy's document ID
//! before it is made, each transaction ID before it is sent. What is left to do is marked
//! (`Filing::pending`) and tried again at every tick, so a wiki or a homeserver that fails
//! holds nothing up for good and a restart repeats nothing.

use super::*;
use crate::{
    commands::When,
    minutes::{self, Receipt},
    state::{Filing, Read, Taken, Withdrawn},
};

/// The reaction to a message that was filed.
pub const FILED: &str = "📥";

/// Messages taken per plenum and day at most (a bridge or a loop cannot flood the wiki).
const TAKES_A_DAY: usize = 10;

/// A meeting waiting for chat minutes reads its page for a result at most this often.
const WAIT_CHECK_EVERY: Duration = Duration::minutes(15);

/// The PDF of a meeting with chat minutes: the page, then their copy.
pub(super) fn pdf_source(page: &str, copy: &str) -> String {
    if copy.trim().is_empty() {
        return page.to_owned();
    }
    format!("{page}\n\n---\n\n## Chat minutes\n\n{copy}")
}

/// How the automatic template message starts once it is shortened.
const TEMPLATE_SHORT: &str = "📋 Template for";

const OFF: &str = "Minutes from the chat are not switched on for this bot (`[minutes] from_chat`): write them on the meeting's page in the wiki.";

const ADOPTED: &str = "✏️ The copy in the wiki was changed there (edited, archived or deleted), so it is the wiki's now: I no longer change it when this message is edited.";

/// A message in a plenum's room as the bot needs it (main.rs reads it from the event).
#[derive(Clone, Debug)]
pub struct ChatMessage {
    pub event: String,
    /// The thread root to answer in.
    pub thread: String,
    pub sender: OwnedUserId,
    pub sender_name: String,
    /// The text, without a reply's fallback and without a leading `!minutes` line.
    pub body: String,
    pub sent_at: DateTime<Utc>,
}

/// The messages a command is about.
#[derive(Default)]
pub struct Origin {
    /// The command's own message (`!minutes` and a line break: it is the minutes).
    pub this: Option<ChatMessage>,
    /// The message the command answers.
    pub replied: Option<ChatMessage>,
}

fn day_key(day: NaiveDate) -> String {
    format!("day:{day}")
}

/// The filing `key` of plenum `p` (it must exist).
fn filing<'a>(st: &'a mut State, p: &Plenum, key: &str) -> &'a mut Filing {
    st.minutes
        .plenums
        .get_mut(&p.key)
        .and_then(|pm| pm.filings.get_mut(key))
        .expect("the filing is in the store")
}

/// The day of `when` not after `today` (without a year: this year's, else last year's).
fn when_day(when: When, today: NaiveDate) -> Option<NaiveDate> {
    match when.year {
        Some(y) => NaiveDate::from_ymd_opt(y, when.month, when.day).filter(|d| *d <= today),
        None => [today.year(), today.year() - 1]
            .into_iter()
            .filter_map(|y| NaiveDate::from_ymd_opt(y, when.month, when.day))
            .find(|d| *d <= today),
    }
}

use chrono::Datelike;

impl<W: Wiki, C: Chat> Engine<W, C> {
    /// A message in a plenum's room that is no command (main.rs): taken when it is minutes
    /// (CHAT-MINUTES.md §3). Not for the bot's own messages, `[members] keep` or a replay (main.rs).
    pub async fn message(&self, room: &RoomId, m: ChatMessage, now: DateTime<Utc>) {
        if !self.minutes.from_chat || !minutes::is_minutes(&m.body) {
            return;
        }
        if self.keep.contains(&m.sender) || self.me.as_ref() == Some(&m.sender) {
            return;
        }
        let mut st = self.state.lock().await;
        let here = self.plenums_in(room);
        let named: Vec<&&Plenum> = here
            .iter()
            .filter(|p| here.len() == 1 || minutes::names_plenum(&m.body, &p.key, &p.name))
            .collect();
        let base = match named.as_slice() {
            [one] => (**one).clone(),
            _ if here.is_empty() => return,
            _ => {
                // several plenums and the first line names none: asked once
                let text = format!(
                    "📥 Minutes for which plenum? Answer the message with `!minutes <key>`. {}",
                    self.keys_hint(room)
                );
                let txn = format!("plenum-which-{}", m.event);
                if let Err(e) = self
                    .chat
                    .reply(room, &m.thread, &m.event, &text, &txn)
                    .await
                {
                    warn!("Asking which plenum minutes are for: {e:#}");
                }
                return;
            }
        };
        let p = self.effective(&st, &base);
        if self.find_taken(&st, &p, &m.event).is_some() {
            return; // delivered twice
        }
        let event = m.event.clone();
        let thread = m.thread.clone();
        match self.take(&mut st, &p, m, None, None, now).await {
            Ok(_) => {}
            Err(why) => {
                info!(plenum = %p.key, %event, "Minutes not taken");
                let txn = format!("plenum-refused-{event}");
                if let Err(e) = self.chat.reply(room, &thread, &event, &why, &txn).await {
                    warn!("Saying why minutes are not taken: {e:#}");
                }
            }
        }
    }

    /// An edit of message `target` (main.rs; its new text, not the fallback). A taken
    /// message follows it when it is its sender's and newer than the version kept; one not
    /// taken that becomes minutes is taken, under the original's ID.
    #[allow(clippy::too_many_arguments)]
    pub async fn edited(
        &self,
        room: &RoomId,
        target: &str,
        sender: &UserId,
        sender_name: &str,
        body: &str,
        sent_at: DateTime<Utc>,
        now: DateTime<Utc>,
    ) {
        if !self.minutes.from_chat {
            return;
        }
        let keys: Vec<&str> = self
            .plenums_in(room)
            .iter()
            .map(|p| p.key.as_str())
            .collect();
        let text = minutes::without_command(body, &keys).to_owned();
        let mut st = self.state.lock().await;
        for base in self.plenums_in(room) {
            let p = self.effective(&st, base);
            let Some((key, i)) = self.find_taken(&st, &p, target) else {
                continue;
            };
            let f = filing(&mut st, &p, &key);
            let t = &mut f.messages[i];
            if t.sender != sender.as_str() {
                info!(plenum = %p.key, event = %target, "An edit by another account: left alone");
                return;
            }
            if sent_at <= t.text_at {
                return; // an older version than the one kept
            }
            t.text_at = sent_at;
            if t.text == text {
                self.save_or_log(&st).await;
                return;
            }
            t.text = text;
            f.pending = true;
            if f.adopted && !f.adopted_told.done {
                f.adopted_due = true;
            }
            if let Err(e) = self.save(&st).await {
                error!("{e:#}");
                return;
            }
            info!(plenum = %p.key, filing = %key, "Chat minutes edited");
            if let Err(e) = self.file(&mut st, &p, &key, now).await {
                warn!(plenum = %p.key, filing = %key, "Filing chat minutes (again at the next tick): {e:#}");
            }
            return;
        }
        drop(st);
        let m = ChatMessage {
            event: target.to_owned(),
            thread: target.to_owned(),
            sender: sender.to_owned(),
            sender_name: sender_name.to_owned(),
            body: text,
            sent_at,
        };
        self.message(room, m, now).await;
    }

    /// A message deleted in the room: withdrawn when it was minutes.
    pub async fn redacted(&self, room: &RoomId, target: &str, now: DateTime<Utc>) {
        let mut st = self.state.lock().await;
        for base in self.plenums_in(room) {
            let p = self.effective(&st, base);
            let Some((key, i)) = self.find_taken(&st, &p, target) else {
                continue;
            };
            let f = filing(&mut st, &p, &key);
            if f.messages[i].withdrawn.is_some() {
                return;
            }
            f.messages[i].withdrawn = Some(Withdrawn::Deleted);
            f.pending = true;
            if let Err(e) = self.save(&st).await {
                error!("{e:#}");
                return;
            }
            info!(plenum = %p.key, filing = %key, "Chat minutes deleted in the room: withdrawn");
            if let Err(e) = self.file(&mut st, &p, &key, now).await {
                warn!(plenum = %p.key, filing = %key, "Filing chat minutes (again at the next tick): {e:#}");
            }
            return;
        }
    }

    /// `(filing, index)` of the message `event` among the plenum's filings.
    fn find_taken(&self, st: &State, p: &Plenum, event: &str) -> Option<(String, usize)> {
        let pm = st.minutes.plenums.get(&p.key)?;
        pm.filings.iter().find_map(|(key, f)| {
            (f.messages.iter())
                .position(|t| t.event == event && t.withdrawn != Some(Withdrawn::Moved))
                .map(|i| (key.clone(), i))
        })
    }

    /// Where minutes go (CHAT-MINUTES.md §4): `(filing key, its day)`. `when`: the day someone gave
    /// (`!minutes <day>`); else the day in the message, else the latest meeting that began.
    fn place(
        &self,
        st: &State,
        p: &Plenum,
        body: &str,
        sent_at: DateTime<Utc>,
        when: Option<When>,
        now: DateTime<Utc>,
    ) -> Result<(String, NaiveDate), String> {
        let today = self.today(now);
        let local_now = now.with_timezone(&self.tz).naive_local();
        let meetings: Vec<(String, MeetingKey)> = st
            .plenums
            .get(&p.key)
            .map(|ps| {
                (ps.meetings.iter())
                    .filter(|(_, m)| m.created && !m.cancelled && !m.skipped)
                    .filter_map(|(id, m)| Some((id.clone(), m.start?)))
                    .collect()
            })
            .unwrap_or_default();
        let (day, meetings) = match when {
            Some(w) => {
                let on: Vec<(String, MeetingKey)> = meetings
                    .into_iter()
                    .filter(|(_, s)| w.matches(*s))
                    .collect();
                let latest = on
                    .iter()
                    .map(|(_, s)| s.date())
                    .filter(|d| *d <= today)
                    .max();
                let day = match (w.year, latest) {
                    (None, Some(d)) => Some(d),
                    _ => when_day(w, today),
                };
                let Some(day) = day else {
                    return Err(format!(
                        "{} is still to come: minutes are filed for a day that has been.",
                        w.describe()
                    ));
                };
                (Some(day), on)
            }
            None => (minutes::day_in(body, today), meetings),
        };
        match minutes::meeting_for(day, &meetings, local_now) {
            Some(id) => {
                let start = meetings
                    .iter()
                    .find(|(m, _)| *m == id)
                    .map(|(_, s)| s.date())
                    .expect("one of them");
                Ok((id, start))
            }
            None => {
                let day = day.unwrap_or_else(|| self.today(sent_at));
                Ok((day_key(day), day))
            }
        }
    }

    /// The message taken as minutes (saved before anything else happens), then filed. Err:
    /// why it is not taken (for the room).
    async fn take(
        &self,
        st: &mut State,
        p: &Plenum,
        m: ChatMessage,
        by: Option<&Sender>,
        when: Option<When>,
        now: DateTime<Utc>,
    ) -> Result<String, String> {
        let (key, day) = self.place(st, p, &m.body, m.sent_at, when, now)?;
        let today = self.today(now);
        let pm = st.minutes.plenums.entry(p.key.clone()).or_default();
        let taken_today = (pm.taken.iter())
            .filter(|at| at.with_timezone(&self.tz).date_naive() == today)
            .count();
        if taken_today >= TAKES_A_DAY {
            warn!(plenum = %p.key, "Ten minutes taken today: no more until tomorrow");
            return Err(format!(
                "⏳ {TAKES_A_DAY} messages were filed as minutes of {} today: no more until \
                 tomorrow.",
                p.name
            ));
        }
        let print = former::fingerprint(&m.body);
        if let Some(f) = pm.filings.get(&key) {
            if f.active().any(|t| former::fingerprint(&t.text) == print) {
                return Err(
                    "📥 This text is filed already (an earlier message has it): nothing new."
                        .to_owned(),
                );
            }
        }
        let f = pm
            .filings
            .entry(key.clone())
            .or_insert_with(|| Filing::new(day));
        f.messages.push(Taken {
            event: m.event.clone(),
            thread: m.thread.clone(),
            sender: m.sender.to_string(),
            sender_name: m.sender_name.clone(),
            taken_by: by.map(|s| s.id.to_string()),
            posted_at: m.sent_at,
            text: m.body.clone(),
            text_at: m.sent_at,
            withdrawn: None,
            reaction: Default::default(),
            reaction_event: None,
            receipt: Default::default(),
            receipt_message: None,
        });
        f.pending = true;
        // a copy that is the wiki's does not take this message: its thread hears it
        if f.adopted && !f.adopted_told.done {
            f.adopted_due = true;
        }
        pm.taken.push(now);
        if let Err(e) = self.save(st).await {
            error!("{e:#}");
            // not saved, not taken: the next delivery or command tries again
            let pm = st.minutes.plenums.get_mut(&p.key).expect("above");
            pm.taken.pop();
            if let Some(f) = pm.filings.get_mut(&key) {
                f.messages.retain(|t| t.event != m.event);
            }
            return Err("⚠️ That could not be saved just now. Please try again.".to_owned());
        }
        info!(plenum = %p.key, filing = %key, event = %m.event, "Minutes from the chat taken");
        if let Err(e) = self.file(st, p, &key, now).await {
            warn!(plenum = %p.key, filing = %key, "Filing chat minutes (again at the next tick): {e:#}");
        }
        Ok(key)
    }

    /// Every filing with something left to do (at every tick). The first error is the
    /// report's.
    pub(super) async fn file_pending(
        &self,
        st: &mut State,
        p: &Plenum,
        now: DateTime<Utc>,
    ) -> Result<()> {
        let keys: Vec<String> = st
            .minutes
            .plenums
            .get(&p.key)
            .map(|pm| {
                (pm.filings.iter())
                    .filter(|(_, f)| f.pending)
                    .map(|(k, _)| k.clone())
                    .collect()
            })
            .unwrap_or_default();
        let mut first = None;
        for key in keys {
            if let Err(e) = self.file(st, p, &key, now).await {
                warn!(plenum = %p.key, filing = %key, "Filing chat minutes (again at the next tick): {e:#}");
                first.get_or_insert(e);
            }
        }
        self.sync_pin(st, p).await;
        first.map_or(Ok(()), Err)
    }

    /// The plenum's latest minutes in the room, from either door: the newest filing's latest
    /// message, or the bot's minutes message of a later meeting.
    fn latest_minutes(&self, st: &State, p: &Plenum) -> Option<String> {
        let pm = st.minutes.plenums.get(&p.key)?;
        let chat = (pm.filings.values())
            .filter(|f| f.has_active())
            .max_by_key(|f| f.day)
            .and_then(|f| {
                let t = f.active().max_by_key(|t| t.posted_at)?;
                Some((f.day, t.event.clone()))
            });
        let wiki = pm.wiki_message.as_ref().map(|w| (w.day, w.event.clone()));
        match (chat, wiki) {
            (Some(c), Some(w)) => Some(if w.0 > c.0 { w.1 } else { c.1 }),
            (c, w) => c.or(w).map(|(_, e)| e),
        }
    }

    /// The latest minutes pinned in the room (CHAT-MINUTES.md §6), the bot's previous pin taken out; never
    /// anyone else's. Only when that changes; a room that does not let the bot is logged once.
    pub(super) async fn sync_pin(&self, st: &mut State, p: &Plenum) {
        if !self.minutes.from_chat || !self.minutes.pin {
            return;
        }
        let wanted = self.latest_minutes(st, p);
        let pm = st.minutes.plenums.entry(p.key.clone()).or_default();
        if wanted == pm.pinned || (wanted.is_some() && wanted == pm.pin_refused) {
            return;
        }
        let unpin: Vec<String> = pm.pinned.iter().cloned().collect();
        match self.chat.set_pins(&p.room, wanted.as_deref(), &unpin).await {
            Ok(true) => {
                info!(plenum = %p.key, "The latest minutes are pinned in the room");
                pm.pinned = wanted;
                pm.pin_refused = None;
            }
            Ok(false) => {
                warn!(plenum = %p.key, "The room does not let the bot pin (it needs the power level for m.room.pinned_events): the minutes stay unpinned");
                pm.pin_refused = wanted;
            }
            Err(e) => {
                warn!(plenum = %p.key, "Pinning the latest minutes (again at the next tick): {e:#}");
                return;
            }
        }
        self.save_or_log(st).await;
    }

    /// The filing's copy written, its messages answered, what they say read and applied.
    async fn file(&self, st: &mut State, p: &Plenum, key: &str, now: DateTime<Utc>) -> Result<()> {
        self.write_copy(st, p, key).await?;
        self.answer_messages(st, p, key).await?;
        let f = filing(st, p, key);
        f.read = (f.active())
            .map(|t| minutes::read(&t.text))
            .fold(Read::default(), |all, r| all.union(&r));
        // a meeting closed already, or none: applied now (else its close does it)
        let open = (st.plenums.get(&p.key))
            .and_then(|ps| ps.meetings.get(key))
            .is_some_and(|m| !m.close.done && !m.cancelled && !m.skipped);
        if !open {
            self.apply(st, p, key, now).await?;
        }
        if filing(st, p, key).has_active() {
            self.shorten_template(st, p, key, true).await?;
        }
        filing(st, p, key).pending = false;
        self.save(st).await
    }

    /// The chat template for meeting `key` (or the next one): the plenum's document, its open
    /// page's topics and the open tasks for a meeting shared in full (CHAT-MINUTES.md §9).
    async fn template_for(&self, st: &State, p: &Plenum, key: &str) -> Result<String> {
        let ps = &st.plenums[&p.key];
        let m = ps.meetings.get(key).context("no such meeting")?;
        let start = m.start.context("a meeting without a start")?;
        let template = match &ps.chat_template_doc_id {
            Some(id) => match self.wiki.doc(id).await? {
                Some(d) => protocol::chat_template_of(&d.text),
                None => protocol::DEFAULT_CHAT_TEMPLATE.to_owned(),
            },
            None => protocol::DEFAULT_CHAT_TEMPLATE.to_owned(),
        };
        let (mut topics, mut tasks) = (Vec::new(), Vec::new());
        if publication(p, m) == Publication::Full {
            if m.created {
                if let Some(page_doc) = self.wiki.doc(&m.doc_id).await? {
                    topics = page::topics(&page_doc.text)
                        .iter()
                        .map(|t| minutes::literal(&t.title()))
                        .collect();
                }
            }
            tasks = (ps.open_tasks.iter())
                .filter(|t| protocol::may_share_task(&t.text, &ps.withheld_tasks))
                .map(|t| t.text.clone())
                .collect();
        }
        Ok(protocol::render_chat_template(
            &template, p, start, &topics, &tasks,
        ))
    }

    /// The template by itself after the reminder, once per meeting (`messages all` only; left
    /// out counts as done).
    pub(super) async fn post_template(&self, st: &mut State, p: &Plenum, key: &str) -> Result<()> {
        let m = meeting(st, p, key);
        if m.template.done {
            return Ok(());
        }
        if !sends(p.messages, Notice::Template) {
            m.template.done = true;
            return self.save(st).await;
        }
        let txn = m.template.txn();
        self.save(st).await?;
        let text = self.template_for(st, p, key).await?;
        let event = self
            .chat
            .post(&p.room, &text, Notice::Template, &txn)
            .await?;
        let m = meeting(st, p, key);
        m.template_message = Some(LinkMessage {
            event,
            text,
            edits: 0,
        });
        m.template.done = true;
        info!(plenum = %p.key, meeting = %key, "Chat template posted");
        self.save(st).await
    }

    /// The meeting's automatic template message edited down to one line, once: when its
    /// minutes are filed, or when it closes without any (never deleted: a deletion leaves a
    /// placeholder, and an edit pings nobody).
    pub(super) async fn shorten_template(
        &self,
        st: &mut State,
        p: &Plenum,
        key: &str,
        filed: bool,
    ) -> Result<()> {
        let Some(m) = st
            .plenums
            .get_mut(&p.key)
            .and_then(|ps| ps.meetings.get_mut(key))
        else {
            return Ok(());
        };
        let when = m.start.map(schedule::describe_meeting).unwrap_or_default();
        let Some(sent) = m.template_message.as_mut() else {
            return Ok(());
        };
        let text = if filed {
            format!(
                "{TEMPLATE_SHORT} {} on {when}. The minutes are filed.",
                p.name
            )
        } else {
            format!(
                "{TEMPLATE_SHORT} {} on {when}. The meeting is closed. `!minutes template` \
                 gives it again.",
                p.name
            )
        };
        // once: shortened already (either way), it stays as it is
        if sent.edits > 0 && sent.text.starts_with(TEMPLATE_SHORT) {
            return Ok(());
        }
        // the same transaction for a try that failed: the homeserver drops a second one
        sent.edits = 1;
        let (event, txn) = (sent.event.clone(), format!("plenum-edit-{}-1", sent.event));
        self.save(st).await?;
        self.chat.edit(&p.room, &event, &text, &txn).await?;
        if let Some(sent) = meeting(st, p, key).template_message.as_mut() {
            sent.text = text;
        }
        self.save(st).await
    }

    /// `!minutes template`: one line, then the template by itself (a message nothing is
    /// before or after, so a phone copies exactly it). For the meeting that is on, else the
    /// next. Answers, not edited later.
    pub(super) async fn template_command(
        &self,
        st: &mut State,
        p: &Plenum,
        now: DateTime<Utc>,
    ) -> String {
        if !self.minutes.from_chat {
            return OFF.to_owned();
        }
        let key = match self.current_meeting(st, p, now) {
            Some(k) => Some(k),
            None => self.next_meeting(st, p, now).map(|(k, _)| k),
        };
        let Some(key) = key else {
            return format!("{} has no meeting in the calendar.", p.name);
        };
        if !st.plenums[&p.key].meetings.contains_key(&key) {
            // a date of the calendar not prepared yet: its start from the calendar
            let ps = st.plenums.get_mut(&p.key).expect("next_meeting made it");
            if let Some(o) = ps.occurrences.iter().find(|o| o.id == key).cloned() {
                ps.meetings.insert(
                    key.clone(),
                    Meeting {
                        doc_id: uuid::Uuid::new_v4().to_string(),
                        start: Some(o.start),
                        end: o.end,
                        ..Default::default()
                    },
                );
            }
        }
        let text = match self.template_for(st, p, &key).await {
            Ok(t) => t,
            Err(e) => {
                warn!(plenum = %p.key, "The chat template: {e:#}");
                return "⚠️ The wiki does not answer just now. Please try again in a moment."
                    .to_owned();
            }
        };
        let when = (st.plenums[&p.key].meetings[&key].start)
            .map(schedule::describe_meeting)
            .unwrap_or_default();
        let line = format!(
            "📋 The template for {} on {when}: copy the next message, fill it in, post it.",
            p.name
        );
        let id = uuid::Uuid::new_v4();
        let sent = async {
            (self.chat)
                .send(&p.room, &line, Notice::Template, &format!("plenum-tl-{id}"))
                .await?;
            (self.chat)
                .post(&p.room, &text, Notice::Template, &format!("plenum-tt-{id}"))
                .await
        };
        match sent.await {
            Ok(_) => String::new(),
            Err(e) => {
                warn!(plenum = %p.key, "Posting the chat template: {e:#}");
                "⚠️ The template could not be posted just now. Please try again.".to_owned()
            }
        }
    }

    /// The meeting's chat minutes, once copied (or adopted): a filing with a message left.
    pub(super) fn chat_minutes_of(&self, st: &State, p: &Plenum, key: &str) -> Option<Filing> {
        if !self.minutes.from_chat {
            return None;
        }
        (st.minutes.plenums.get(&p.key))
            .and_then(|pm| pm.filings.get(key))
            .filter(|f| f.has_active() && (f.created || f.adopted))
            .cloned()
    }

    /// The page's outcome with the chat minutes' (CHAT-MINUTES.md §1: the union): their decisions and tasks
    /// added, the tasks a done line closes taken out, and those closed from the chat before
    /// (`done_tasks`) too. The tasks closed now, for `done_tasks`.
    pub(super) fn merge_chat(
        &self,
        st: &State,
        p: &Plenum,
        outcome: &mut protocol::Outcome,
        chat: Option<&Filing>,
    ) -> Vec<String> {
        let mut closed = Vec::new();
        if let Some(f) = chat {
            for d in &f.read.decisions {
                if !outcome.decisions.contains(d) {
                    outcome.decisions.push(d.clone());
                }
            }
            for t in &f.read.tasks {
                if !outcome.tasks.contains(t) {
                    outcome.tasks.push(t.clone());
                }
            }
            let (done, open): (Vec<String>, Vec<String>) = std::mem::take(&mut outcome.tasks)
                .into_iter()
                .partition(|t| f.read.done.iter().any(|d| minutes::closes(d, t)));
            outcome.tasks = open;
            closed = done;
        }
        if let Some(ps) = st.plenums.get(&p.key) {
            (outcome.tasks).retain(|t| !ps.done_tasks.iter().any(|d| minutes::same_task(d, t)));
        }
        closed
    }

    /// The postponed lines of chat minutes that name no topic of the page: topics of their own
    /// for the next page (once: one already waiting is not added again).
    #[allow(clippy::too_many_arguments)]
    pub(super) fn keep_postponed(
        &self,
        st: &mut State,
        p: &Plenum,
        postponed: &[String],
        topics: &[page::Topic],
        from: MeetingKey,
        link: &str,
        shared: Publication,
    ) {
        let ps = st.plenums.entry(p.key.clone()).or_default();
        for line in postponed {
            if topics
                .iter()
                .any(|t| page::postponed_names(std::slice::from_ref(line), t))
            {
                continue;
            }
            let block = page::postponed_block(line, from.date(), link);
            let heading = page::block_heading(&block).to_owned();
            if ps.open_topics.iter().any(|t| t.heading == heading) {
                continue;
            }
            ps.open_topics.push(CarriedTopic {
                heading,
                block,
                from,
                publication: shared,
            });
        }
    }

    /// The close of a meeting with chat minutes is done: what of them it applied, and the link
    /// to their copy on the page (in the result box, else a line of its own).
    pub(super) async fn closed_with_chat(
        &self,
        st: &mut State,
        p: &Plenum,
        key: &str,
        page_doc: &Doc,
        f: &Filing,
        link: &str,
    ) -> Result<()> {
        if !f.noted_on_page {
            if let Some(url) = f.url.as_deref().filter(|u| !page_doc.text.contains(*u)) {
                let title = f.title.clone().unwrap_or_default();
                let line = format!(
                    "\n\n📥 Chat minutes: [{}]({})",
                    minutes::literal(&title),
                    self.wiki.link(url)
                );
                self.wiki.append(&page_doc.id, &line).await?;
            }
        }
        let fm = filing(st, p, key);
        fm.applied = Some(f.read.clone());
        fm.listed = Some(f.read.decisions.clone());
        fm.applied_link = Some(link.to_owned());
        fm.noted_on_page = true;
        self.save(st).await
    }

    /// Past its closing time, a meeting whose page has no result and no decision waits for
    /// chat minutes (CHAT-MINUTES.md §7), at most `wait_days` after its end; the page is read at most every
    /// [`WAIT_CHECK_EVERY`].
    pub(super) async fn waits_for_minutes(
        &self,
        st: &mut State,
        p: &Plenum,
        key: &str,
        end: DateTime<Utc>,
        close_at: DateTime<Utc>,
        now: DateTime<Utc>,
    ) -> Result<bool> {
        if !self.minutes.from_chat || self.minutes.wait_days == 0 {
            return Ok(false);
        }
        let until = close_at.max(end + Duration::days(i64::from(self.minutes.wait_days)));
        if now >= until {
            return Ok(false);
        }
        let m = meeting(st, p, key);
        if m.close
            .wait_checked
            .is_some_and(|at| now - at < WAIT_CHECK_EVERY)
        {
            return Ok(true);
        }
        let doc_id = m.doc_id.clone();
        let Some(doc) = self.wiki.doc(&doc_id).await? else {
            return Ok(false); // the close says the page is gone
        };
        let written = !protocol::extract(&doc.text).decisions.is_empty()
            || page::topics(&doc.text).iter().any(|t| !t.reading.carried());
        if written {
            return Ok(false);
        }
        if m.close.wait_checked.is_none() {
            info!(plenum = %p.key, meeting = %key, "No result on the page: waiting for minutes from the chat");
        }
        meeting(st, p, key).close.wait_checked = Some(now);
        self.save(st).await?;
        Ok(true)
    }

    /// Once, `nudge_days` after the end of a meeting that waits for its minutes: the room
    /// hears that none came (`messages all` only; left out counts as done).
    pub(super) async fn nudge(
        &self,
        st: &mut State,
        p: &Plenum,
        key: &str,
        end: DateTime<Utc>,
        now: DateTime<Utc>,
    ) -> Result<()> {
        let days = self.minutes.nudge_days;
        let m = meeting(st, p, key);
        if days == 0 || m.close.nudge.done || now < end + Duration::days(i64::from(days)) {
            return Ok(());
        }
        if !sends(p.messages, Notice::MinutesMissing) {
            m.close.nudge.done = true;
            return self.save(st).await;
        }
        let txn = m.close.nudge.txn();
        let start = m.start.context("a meeting without a start")?;
        let link = (publication(p, m) != Publication::Confidential).then(|| self.link_of(m));
        self.save(st).await?;
        let text = minutes::nudge(&p.name, start, link.as_deref());
        self.chat
            .send(&p.room, &text, Notice::MinutesMissing, &txn)
            .await?;
        meeting(st, p, key).close.nudge.done = true;
        info!(plenum = %p.key, meeting = %key, "No minutes yet: the room was reminded");
        self.save(st).await
    }

    /// Chat minutes of a meeting closed already, or of none (CHAT-MINUTES.md §7 "After the
    /// close"): the plenum's lists follow what they say now. Decisions replace theirs (a block
    /// in the decision list when they changed, "updated" after the first), tasks join or leave
    /// the open ones and done lines close them, new postponed topics wait for the next page; a
    /// closed page gets the link to the copy once.
    async fn apply(&self, st: &mut State, p: &Plenum, key: &str, now: DateTime<Utc>) -> Result<()> {
        let f = filing(st, p, key).clone();
        let new = if f.has_active() {
            f.read.clone()
        } else {
            Read::default()
        };
        let m = (st.plenums.get(&p.key))
            .and_then(|ps| ps.meetings.get(key))
            .filter(|m| m.created && m.close.done)
            .cloned();
        let copy_link = f.url.as_deref().map(|u| self.wiki.link(u));
        let link = match (&f.applied_link, &m, &copy_link) {
            (Some(l), ..) => l.clone(),
            (None, Some(m), _) => self.link_of(m),
            (None, None, Some(l)) => l.clone(),
            // nothing was applied and there is no copy (withdrawn before it was made)
            (None, None, None) => return Ok(()),
        };
        let old = f.applied.clone().unwrap_or_default();
        let shared = m.as_ref().map_or(p.publication, |m| publication(p, m));
        let day = f.day;
        // a closed page: the link to the copy, once
        if let (Some(m), Some(copy), false) = (&m, &copy_link, f.noted_on_page) {
            if f.has_active() {
                if let Some(page_doc) = self.wiki.doc(&m.doc_id).await? {
                    let path = f.url.clone().unwrap_or_default();
                    if !page_doc.text.contains(&path) {
                        let title = f.title.clone().unwrap_or_default();
                        let line = format!(
                            "\n\n📥 Chat minutes: [{}]({copy})",
                            minutes::literal(&title)
                        );
                        self.wiki.append(&m.doc_id, &line).await?;
                    }
                }
                filing(st, p, key).noted_on_page = true;
                self.save(st).await?;
            }
        }
        // the decision list: a block when the decisions changed
        let listed_now = f.listed.clone().unwrap_or_default();
        if new.decisions != listed_now {
            let (_, _, decisions) = self.setup(st, p).await?;
            let title = (f.title.clone()).unwrap_or_else(|| format!("{} {}", p.name, day));
            let first = f.listed.is_none();
            // with the time: two changes on one day are two blocks
            let at = format!(
                "{}, {}",
                schedule::describe_day(self.today(now)),
                now.with_timezone(&self.tz).format("%H:%M")
            );
            let heading = if first {
                format!("### [{}]({link})", minutes::literal(&title))
            } else if !f.has_active() {
                format!(
                    "### [{}]({link}) (withdrawn {at})",
                    minutes::literal(&title)
                )
            } else {
                format!("### [{}]({link}) (updated {at})", minutes::literal(&title))
            };
            let mut block = format!("{heading}\n\n");
            if new.decisions.is_empty() {
                block.push_str("No decisions any more.\n");
            }
            for d in &new.decisions {
                let d = d.replace('[', "\\[").replace(']', "\\]");
                block.push_str(&format!("* {d}\n"));
            }
            // after a crash between the append and the save, the block is there already
            if !decisions.text.contains(&heading[4..]) {
                self.wiki
                    .append(&decisions.id, &format!("\n\n{block}"))
                    .await?;
            }
            filing(st, p, key).listed = Some(new.decisions.clone());
            self.save(st).await?;
        }
        let ps = st.plenums.entry(p.key.clone()).or_default();
        let carried = |text: &String| Carried {
            text: text.clone(),
            date: day,
            link: link.clone(),
        };
        // the overview's latest decisions
        (ps.decisions).retain(|d| !(d.link == link && old.decisions.contains(&d.text)));
        ps.decisions.extend(new.decisions.iter().map(carried));
        ps.decisions.sort_by_key(|d| d.date);
        // the open tasks
        (ps.open_tasks).retain(|t| !(old.tasks.contains(&t.text) && !new.tasks.contains(&t.text)));
        // only those new in the minutes: one closed since (on a later page) stays closed
        for t in new.tasks.iter().filter(|t| !old.tasks.contains(t)) {
            if !ps.open_tasks.iter().any(|o| &o.text == t) {
                ps.open_tasks.push(carried(t));
            }
            if shared != Publication::Full && !ps.withheld_tasks.contains(t) {
                ps.withheld_tasks.push(t.clone());
            }
        }
        let closed: Vec<String> = (ps.open_tasks.iter())
            .filter(|t| new.done.iter().any(|d| minutes::closes(d, &t.text)))
            .map(|t| t.text.clone())
            .collect();
        ps.open_tasks.retain(|t| !closed.contains(&t.text));
        for t in closed {
            if !ps.done_tasks.contains(&t) {
                ps.done_tasks.push(t);
            }
        }
        // new postponed topics
        let from = match &m {
            Some(m) => m.start.context("a meeting without a start")?,
            None => MeetingKey(day.and_time(chrono::NaiveTime::MIN)),
        };
        let fresh: Vec<String> = (new.postponed.iter())
            .filter(|l| !old.postponed.contains(l))
            .cloned()
            .collect();
        self.keep_postponed(st, p, &fresh, &[], from, &link, shared);
        // the new tasks onto the next page when it is open already: its close reads the open
        // tasks from it, and would drop what it never had (before `applied` is saved, so a
        // wiki that fails here is tried again)
        let added: Vec<Carried> = (st.plenums[&p.key].open_tasks.iter())
            .filter(|t| new.tasks.contains(&t.text) && !old.tasks.contains(&t.text))
            .cloned()
            .collect();
        self.tasks_onto_next_page(st, p, from, &added).await?;
        let fm = filing(st, p, key);
        fm.applied = Some(new);
        fm.applied_link = Some(link);
        info!(plenum = %p.key, filing = %key, "Chat minutes applied to the plenum's lists");
        self.save(st).await
    }

    /// The copy's title: the meeting's page's, else by the day.
    fn copy_title(&self, st: &State, p: &Plenum, key: &str, f: &Filing) -> String {
        let page = (st.plenums.get(&p.key))
            .and_then(|ps| ps.meetings.get(key))
            .filter(|m| m.created)
            .and_then(|m| m.title.clone());
        match page {
            Some(t) => format!("{}: Chat minutes", t.trim_end_matches(CANCELLED)),
            None => format!("{} {}: Chat minutes", p.name, f.day.format("%Y-%m-%d")),
        }
    }

    /// `(collection, parent)` for a new copy: below the meeting's page, else below
    /// "<Name>: Minutes".
    async fn copy_place(&self, st: &mut State, p: &Plenum, key: &str) -> Result<(String, String)> {
        let ready = st
            .plenums
            .get(&p.key)
            .is_some_and(|ps| ps.collection_id.is_some() && ps.protocols_doc_id.is_some());
        if !ready {
            self.setup(st, p).await?;
        }
        let ps = &st.plenums[&p.key];
        let collection = ps.collection_id.clone().context("no collection")?;
        let protocols = ps.protocols_doc_id.clone().context("no minutes document")?;
        let page = (ps.meetings.get(key))
            .filter(|m| m.created)
            .map(|m| m.doc_id.clone());
        if let Some(page) = page {
            if self.wiki.doc(&page).await?.is_some() {
                return Ok((collection, page));
            }
        }
        Ok((collection, protocols))
    }

    /// The copy as the messages say now (CHAT-MINUTES.md §2): made, replaced while it is still the
    /// bot's text, left alone once someone changed or deleted it, into the trash when no
    /// message is left.
    async fn write_copy(&self, st: &mut State, p: &Plenum, key: &str) -> Result<()> {
        let f = filing(st, p, key).clone();
        if !f.has_active() {
            if f.created {
                if !f.adopted {
                    self.wiki.delete(&f.doc_id).await?;
                    info!(plenum = %p.key, filing = %key, "Chat minutes withdrawn: their copy is in the wiki's trash");
                }
                // a message taken later gets a copy of its own
                *filing(st, p, key) = Filing {
                    messages: f.messages,
                    listed: f.listed,
                    applied: f.applied,
                    noted_on_page: f.noted_on_page,
                    pending: true,
                    ..Filing::new(f.day)
                };
                self.save(st).await?;
            }
            return Ok(());
        }
        if f.adopted {
            return Ok(());
        }
        let active: Vec<&Taken> = f.active().collect();
        let text = minutes::copy_text(&active, self.tz);
        let sent = minutes::exact(&text);
        let title = self.copy_title(st, p, key, &f);
        if !f.created {
            let doc = match self.wiki.doc(&f.doc_id).await? {
                // made before a restart: compared and written below
                Some(d) => {
                    filing(st, p, key).sent = None;
                    d
                }
                None => {
                    let (collection, parent) = self.copy_place(st, p, key).await?;
                    info!(plenum = %p.key, filing = %key, "Creating the copy of chat minutes");
                    let d = self
                        .wiki
                        .create_doc(&f.doc_id, &collection, Some(&parent), &title, &text)
                        .await?;
                    filing(st, p, key).sent = Some(sent);
                    d
                }
            };
            let fm = filing(st, p, key);
            fm.created = true;
            fm.url = Some(doc.url.clone());
            fm.title = Some(doc.title.clone());
            // what the wiki gave back: Outline rewrites Markdown
            fm.written = Some(former::fingerprint(&doc.text));
            self.save(st).await?;
        }
        let f = filing(st, p, key).clone();
        if f.sent == Some(sent) && f.title.as_deref() == Some(title.as_str()) {
            return Ok(());
        }
        let Some(doc) = self.wiki.doc(&f.doc_id).await? else {
            info!(plenum = %p.key, filing = %key, "The copy of chat minutes was deleted in the wiki: not made again");
            filing(st, p, key).adopted = true;
            // a change of the message did not reach the copy: its thread hears it once
            filing(st, p, key).adopted_due = f.sent != Some(sent);
            return self.save(st).await;
        };
        // archived: Outline refuses every write (403), which would fail at every tick
        if doc.archived {
            info!(plenum = %p.key, filing = %key, "The copy of chat minutes was archived in the wiki: it is left as it is");
            filing(st, p, key).adopted = true;
            // a change of the message did not reach the copy: its thread hears it once
            filing(st, p, key).adopted_due = f.sent != Some(sent);
            return self.save(st).await;
        }
        if f.written != Some(former::fingerprint(&doc.text)) {
            info!(plenum = %p.key, filing = %key, "The copy of chat minutes was changed in the wiki: it is left as it is");
            filing(st, p, key).adopted = true;
            // a change of the message did not reach the copy: its thread hears it once
            filing(st, p, key).adopted_due = f.sent != Some(sent);
            return self.save(st).await;
        }
        if f.sent != Some(sent) {
            self.wiki.replace(&doc.id, &text).await?;
            let back = (self.wiki.doc(&doc.id).await?)
                .context("the copy vanished while it was written")?;
            let fm = filing(st, p, key);
            fm.written = Some(former::fingerprint(&back.text));
            fm.sent = Some(sent);
            self.save(st).await?;
            info!(plenum = %p.key, filing = %key, "The copy of chat minutes follows its messages");
        }
        if doc.title != title {
            self.wiki.rename(&doc.id, &title).await?;
            filing(st, p, key).title = Some(title);
            self.save(st).await?;
        }
        Ok(())
    }

    /// The thread reply to message `i` of a filing as it is now.
    fn receipt_text(&self, st: &State, p: &Plenum, key: &str, i: usize) -> String {
        let f = &st.minutes.plenums[&p.key].filings[key];
        let t = &f.messages[i];
        let m = st.plenums.get(&p.key).and_then(|ps| ps.meetings.get(key));
        let shared = m.map_or(p.publication, |m| publication(p, m));
        let link = (shared != Publication::Confidential)
            .then(|| f.url.as_deref().map(|u| self.wiki.link(u)))
            .flatten();
        let read = minutes::read(&t.text);
        minutes::receipt(&Receipt {
            plenum: &p.name,
            meeting: m.and_then(|m| m.start).map(schedule::describe_meeting),
            day: f.day,
            link,
            read: &read,
            house_reads: p.access == Access::Members,
            withdrawn: t.withdrawn.is_some(),
        })
    }

    /// Each message's reaction and thread reply (CHAT-MINUTES.md §6), once; a reply whose text changed is
    /// edited (pings nobody). A copy that became the wiki's is said once.
    async fn answer_messages(&self, st: &mut State, p: &Plenum, key: &str) -> Result<()> {
        let n = filing(st, p, key).messages.len();
        for i in 0..n {
            let t = filing(st, p, key).messages[i].clone();
            if t.withdrawn.is_none() && !t.reaction.done {
                let txn = filing(st, p, key).messages[i].reaction.txn();
                self.save(st).await?;
                let reaction = self.chat.react(&p.room, &t.event, FILED, &txn).await?;
                let tm = &mut filing(st, p, key).messages[i];
                tm.reaction.done = true;
                tm.reaction_event = Some(reaction);
                self.save(st).await?;
            }
            // `undo`: the message is no minutes, so the mark on it goes (a deleted message has
            // none any more, a moved one keeps its own)
            if let (Some(Withdrawn::Undone), Some(reaction)) = (t.withdrawn, &t.reaction_event) {
                let txn = format!("plenum-unreact-{reaction}");
                self.chat.unreact(&p.room, reaction, &txn).await?;
                let tm = &mut filing(st, p, key).messages[i];
                tm.reaction_event = None;
                // taken again later, it is marked anew
                tm.reaction = Default::default();
                self.save(st).await?;
            }
            let text = self.receipt_text(st, p, key, i);
            match t.receipt_message {
                // never answered (or moved on, with its reply): nothing to say
                None if t.withdrawn.is_some() => {}
                None => {
                    let txn = filing(st, p, key).messages[i].receipt.txn();
                    self.save(st).await?;
                    let event = (self.chat)
                        .reply(&p.room, &t.thread, &t.event, &text, &txn)
                        .await?;
                    let tm = &mut filing(st, p, key).messages[i];
                    tm.receipt_message = Some(LinkMessage {
                        event,
                        text,
                        edits: 0,
                    });
                    tm.receipt.done = true;
                    self.save(st).await?;
                }
                Some(mut sent) if sent.text != text => {
                    sent.edits += 1;
                    let txn = format!("plenum-edit-{}-{}", sent.event, sent.edits);
                    filing(st, p, key).messages[i].receipt_message = Some(sent.clone());
                    self.save(st).await?;
                    self.chat.edit(&p.room, &sent.event, &text, &txn).await?;
                    sent.text = text;
                    filing(st, p, key).messages[i].receipt_message = Some(sent);
                    self.save(st).await?;
                }
                Some(_) => {}
            }
        }
        let f = filing(st, p, key);
        if f.adopted && f.adopted_due && !f.adopted_told.done {
            let Some(last) = f.active().max_by_key(|t| t.text_at).cloned() else {
                return Ok(());
            };
            let txn = f.adopted_told.txn();
            self.save(st).await?;
            (self.chat)
                .reply(&p.room, &last.thread, &last.event, ADOPTED, &txn)
                .await?;
            filing(st, p, key).adopted_told.done = true;
            self.save(st).await?;
        }
        Ok(())
    }

    /// The plenum a minutes command is for: the named one, the room's only one, or the one the
    /// minutes name in their first line.
    fn plenum_for(
        &self,
        st: &State,
        room: &RoomId,
        key: Option<&str>,
        body: Option<&str>,
    ) -> Result<Plenum, String> {
        let here = self.plenums_in(room);
        if key.is_none() && here.len() > 1 {
            if let Some(body) = body {
                let named: Vec<&&Plenum> = (here.iter())
                    .filter(|p| minutes::names_plenum(body, &p.key, &p.name))
                    .collect();
                if let [one] = named.as_slice() {
                    return Ok(self.effective(st, one));
                }
            }
        }
        self.pick(room, key).map(|p| self.effective(st, p))
    }

    /// `!minutes` as a reply (with a day: for the meeting of that day), or `!minutes` and a
    /// line break. The answer is empty when the reaction and the thread reply say it all.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn take_command(
        &self,
        st: &mut State,
        room: &RoomId,
        sender: &Sender,
        key: Option<&str>,
        when: Option<When>,
        origin: Origin,
        now: DateTime<Utc>,
    ) -> String {
        if !self.minutes.from_chat {
            return OFF.to_owned();
        }
        let (m, by) = match (origin.this, origin.replied) {
            (Some(this), _) => (this, None),
            (None, Some(replied)) => {
                let by = (replied.sender != sender.id).then_some(sender);
                (replied, by)
            }
            (None, None) => {
                return "Answer the minutes message with `!minutes` (or `!minutes <day>` for the \
                        meeting of that day), or write `!minutes`, a line break and the minutes."
                    .to_owned()
            }
        };
        let p = match self.plenum_for(st, room, key, Some(&m.body)) {
            Ok(p) => p,
            Err(e) => return e,
        };
        if let Some((old, i)) = self.find_taken(st, &p, &m.event) {
            let t = st.minutes.plenums[&p.key].filings[&old].messages[i].clone();
            if when.is_some() {
                return self.move_minutes(st, &p, &old, i, when, now).await;
            }
            if t.withdrawn.is_none() {
                return "📥 That message is filed already (see its thread).".to_owned();
            }
            // withdrawn before: taken again
            let tm = &mut filing(st, &p, &old).messages[i];
            tm.withdrawn = None;
            tm.taken_by = by.map(|s| s.id.to_string()).or(tm.taken_by.take());
            filing(st, &p, &old).pending = true;
            if let Err(e) = self.save(st).await {
                error!("{e:#}");
                return "⚠️ That could not be saved just now. Please try again.".to_owned();
            }
            if let Err(e) = self.file(st, &p, &old, now).await {
                warn!(plenum = %p.key, filing = %old, "Filing chat minutes (again at the next tick): {e:#}");
            }
            return String::new();
        }
        match self.take(st, &p, m, by, when, now).await {
            Ok(key) if st.minutes.plenums[&p.key].filings[&key].pending => {
                "📥 Noted. The wiki does not answer just now: the minutes are filed as soon as it does."
                    .to_owned()
            }
            Ok(_) => String::new(),
            Err(why) => why,
        }
    }

    /// `!minutes <day>` as a reply to filed minutes: they move to the meeting of that day;
    /// both copies are written anew.
    async fn move_minutes(
        &self,
        st: &mut State,
        p: &Plenum,
        old: &str,
        i: usize,
        when: Option<When>,
        now: DateTime<Utc>,
    ) -> String {
        let t = st.minutes.plenums[&p.key].filings[old].messages[i].clone();
        let (key, day) = match self.place(st, p, &t.text, t.posted_at, when, now) {
            Ok(place) => place,
            Err(why) => return why,
        };
        if key == old {
            return "📥 They are filed there already.".to_owned();
        }
        {
            let tm = &mut filing(st, p, old).messages[i];
            tm.withdrawn = Some(Withdrawn::Moved);
            // the thread reply goes along (edited there), the old filing says nothing
            tm.receipt_message = None;
            tm.receipt.done = true;
        }
        filing(st, p, old).pending = true;
        let pm = st.minutes.plenums.get_mut(&p.key).expect("above");
        let f = pm
            .filings
            .entry(key.clone())
            .or_insert_with(|| Filing::new(day));
        f.messages.push(Taken {
            withdrawn: None,
            ..t
        });
        f.pending = true;
        if let Err(e) = self.save(st).await {
            error!("{e:#}");
            return "⚠️ That could not be saved just now. Please try again.".to_owned();
        }
        info!(plenum = %p.key, from = %old, to = %key, "Chat minutes moved");
        for k in [old, key.as_str()] {
            if let Err(e) = self.file(st, p, k, now).await {
                warn!(plenum = %p.key, filing = %k, "Filing chat minutes (again at the next tick): {e:#}");
            }
        }
        String::new()
    }

    /// `!minutes undo`: the message it answers (else the sender's latest minutes) is no
    /// minutes. For its sender, who took it, the organizers and the admins.
    pub(super) async fn undo_command(
        &self,
        st: &mut State,
        room: &RoomId,
        sender: &Sender,
        key: Option<&str>,
        replied: Option<ChatMessage>,
        now: DateTime<Utc>,
    ) -> String {
        if !self.minutes.from_chat {
            return OFF.to_owned();
        }
        let plenums: Vec<Plenum> = match key {
            Some(_) => match self.pick(room, key) {
                Ok(p) => vec![self.effective(st, p)],
                Err(e) => return e,
            },
            None => (self.plenums_in(room).into_iter())
                .map(|p| self.effective(st, p))
                .collect(),
        };
        let me = sender.id.as_str();
        let mut found: Option<(Plenum, String, usize, DateTime<Utc>)> = None;
        for p in &plenums {
            let Some(pm) = st.minutes.plenums.get(&p.key) else {
                continue;
            };
            for (k, f) in &pm.filings {
                for (i, t) in f.messages.iter().enumerate() {
                    let hit = match &replied {
                        Some(r) => t.event == r.event && t.withdrawn != Some(Withdrawn::Moved),
                        None => {
                            t.withdrawn.is_none()
                                && (t.sender == me || t.taken_by.as_deref() == Some(me))
                        }
                    };
                    if hit && found.as_ref().is_none_or(|(.., at)| t.posted_at > *at) {
                        found = Some((p.clone(), k.clone(), i, t.posted_at));
                    }
                }
            }
        }
        let Some((p, k, i, _)) = found else {
            return "There are no minutes of yours to withdraw here. Answer the minutes message \
                    with `!minutes undo`."
                .to_owned();
        };
        let t = st.minutes.plenums[&p.key].filings[&k].messages[i].clone();
        let allowed = t.sender == me
            || t.taken_by.as_deref() == Some(me)
            || p.organizers.contains(&sender.id)
            || self.admins.contains(&sender.id);
        if !allowed {
            return "⛔ Only who posted the minutes, who filed them, the organizers or the admins \
                    withdraw them."
                .to_owned();
        }
        if t.withdrawn.is_some() {
            return "That message is withdrawn already.".to_owned();
        }
        filing(st, &p, &k).messages[i].withdrawn = Some(Withdrawn::Undone);
        filing(st, &p, &k).pending = true;
        if let Err(e) = self.save(st).await {
            error!("{e:#}");
            filing(st, &p, &k).messages[i].withdrawn = None;
            return "⚠️ That could not be saved just now. Please try again.".to_owned();
        }
        info!(plenum = %p.key, filing = %k, "Chat minutes withdrawn (!minutes undo)");
        if let Err(e) = self.file(st, &p, &k, now).await {
            warn!(plenum = %p.key, filing = %k, "Filing chat minutes (again at the next tick): {e:#}");
        }
        "↩️ Withdrawn: that message is no longer filed as minutes.".to_owned()
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::super::fakes::*;
    use super::*;
    use crate::state::Carried;
    use chrono::TimeZone;

    pub fn plenum(key: &str, name: &str, room: &str) -> Plenum {
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
            messages: Messages::All,
            room_settings: true,
            page_after_days: None,
        }
    }

    pub struct T {
        pub engine: Engine<FakeWiki, FakeChat>,
        pub dir: tempfile::TempDir,
    }

    /// An engine with chat minutes switched on, no PDF, and the plenums' calendars: the 1st
    /// and the 15th of November and December 2026, 19:00 to 21:00.
    pub fn setup(plenums: Vec<Plenum>) -> T {
        let dir = tempfile::tempdir().unwrap();
        let pdf = PdfConfig {
            enabled: false,
            font_dir: String::new(),
            post_to_room: false,
        };
        let mut engine = Engine::new(
            plenums,
            "Europe/Berlin".parse().unwrap(),
            FakeWiki::default(),
            FakeChat::default(),
            pdf,
            HashSet::from(["@admin:example.org".try_into().unwrap()]),
            State::default(),
            dir.path().join("state.json"),
        );
        engine.minutes.from_chat = true;
        {
            let mut st = engine.state.try_lock().unwrap();
            for p in &engine.plenums {
                let ps = st.plenums.entry(p.key.clone()).or_default();
                ps.occurrences = ["2026-11-01", "2026-11-15", "2026-12-01", "2026-12-15"]
                    .iter()
                    .map(|d| occ(&format!("{}#{d}", p.key), &format!("{d} 19:00")))
                    .collect();
                ps.calendar_read = Some(at(2026, 10, 1, 0, 0));
            }
        }
        T { engine, dir }
    }

    pub fn occ(id: &str, start: &str) -> Occurrence {
        let start: MeetingKey = start.parse().unwrap();
        Occurrence {
            id: id.into(),
            start,
            end: Some(start.0 + Duration::hours(2)),
        }
    }

    /// Berlin local time.
    pub fn at(y: i32, m: u32, d: u32, h: u32, min: u32) -> DateTime<Utc> {
        let tz: Tz = "Europe/Berlin".parse().unwrap();
        tz.with_ymd_and_hms(y, m, d, h, min, 0)
            .unwrap()
            .with_timezone(&Utc)
    }

    pub fn room() -> OwnedRoomId {
        "!house:example.org".try_into().unwrap()
    }

    pub fn msg(event: &str, who: &str, body: &str, sent: DateTime<Utc>) -> ChatMessage {
        ChatMessage {
            event: event.into(),
            thread: event.into(),
            sender: format!("@{}:example.org", who.to_lowercase())
                .try_into()
                .unwrap(),
            sender_name: who.into(),
            body: body.into(),
            sent_at: sent,
        }
    }

    pub fn person(who: &str) -> Sender {
        Sender {
            id: format!("@{}:example.org", who.to_lowercase())
                .try_into()
                .unwrap(),
            name: who.into(),
        }
    }

    pub const SAMPLE: &str = "\
Protokoll Hausplenum 15.11.
Anwesend: Anna, Ben, Cara

Fahrradkeller: zu voll, jedes Rad bekommt ein Namensschild.
Beschluss: Räder ohne Schild kommen im Januar in die Werkstatt.
Aufgabe: Anna besorgt Schilder bis 30.11.
Vertagt: Werkzeugregal";

    /// The pages up to the meeting of 15 November are made (the November meetings are
    /// prepared three days before).
    pub async fn pages_made(e: &Engine<FakeWiki, FakeChat>) {
        e.tick(at(2026, 11, 12, 19, 0)).await;
    }

    pub fn copies(e: &Engine<FakeWiki, FakeChat>) -> Vec<Doc> {
        (e.wiki.docs.lock().unwrap().iter())
            .filter(|(d, del)| !del && d.title.ends_with(": Chat minutes"))
            .map(|(d, _)| d.clone())
            .collect()
    }

    pub fn doc_by_id(e: &Engine<FakeWiki, FakeChat>, id: &str) -> Option<Doc> {
        (e.wiki.docs.lock().unwrap().iter())
            .find(|(d, del)| d.id == id && !del)
            .map(|(d, _)| d.clone())
    }

    pub fn replies(e: &Engine<FakeWiki, FakeChat>) -> Vec<String> {
        (e.chat.replies.lock().unwrap().iter())
            .map(|r| r.3.clone())
            .collect()
    }

    pub fn reactions(e: &Engine<FakeWiki, FakeChat>) -> usize {
        e.chat.reactions.lock().unwrap().len()
    }

    /// A restart: a new engine from the files, the same wiki and chat.
    pub async fn restart(t: &mut T) {
        let path = t.dir.path().join("state.json");
        let state = State::load(&path).await.unwrap();
        let old = &mut t.engine;
        let wiki = std::mem::take(&mut old.wiki);
        let chat = std::mem::take(&mut old.chat);
        let minutes = old.minutes.clone();
        let mut engine = Engine::new(
            old.plenums.clone(),
            old.tz,
            wiki,
            chat,
            old.pdf.clone(),
            old.admins.clone(),
            state,
            path,
        );
        engine.minutes = minutes;
        t.engine = engine;
    }

    async fn take_reply(
        e: &Engine<FakeWiki, FakeChat>,
        who: &Sender,
        cmd: &str,
        replied: ChatMessage,
        now: DateTime<Utc>,
    ) -> String {
        let keys: Vec<&str> = e.plenums.iter().map(|p| p.key.as_str()).collect();
        let cmd = crate::commands::parse_for(cmd, &keys).unwrap();
        let origin = Origin {
            this: None,
            replied: Some(replied),
        };
        e.answer(&room(), who, cmd, origin, now, Scope::Room)
            .await
            .join("\n\n")
    }

    #[tokio::test]
    async fn a_minutes_message_makes_one_copy_one_reaction_and_one_reply() {
        let t = setup(vec![plenum("house", "House plenum", "!house:example.org")]);
        let e = &t.engine;
        pages_made(e).await;
        let now = at(2026, 11, 16, 10, 12);
        e.message(&room(), msg("$m1", "Anna", SAMPLE, now), now)
            .await;
        let c = copies(e);
        assert_eq!(c.len(), 1, "{c:?}");
        assert_eq!(c[0].title, "House plenum 2026-11-15: Chat minutes");
        let page = e.wiki.by_title("House plenum 2026-11-15").unwrap();
        assert_eq!(
            c[0].parent_id.as_deref(),
            Some(page.id.as_str()),
            "below the page"
        );
        assert!(c[0]
            .text
            .contains("Copy of the minutes Anna posted in the chat on 16 Nov 2026, 10:12."));
        assert!(c[0].text.contains("Beschluss: Räder ohne Schild"));
        assert_eq!(reactions(e), 1);
        let r = replies(e);
        assert_eq!(r.len(), 1);
        assert!(r[0].starts_with("📥 Filed as the minutes of House plenum, Sunday, 15 November 2026 at 19:00: https://wiki.test/doc/"), "{}", r[0]);
        assert!(r[0].contains("Decisions (1)") && r[0].contains("Postponed (1): Werkzeugregal"));
        let (_, thread, event, _, _) = e.chat.replies.lock().unwrap()[0].clone();
        assert_eq!((thread.as_str(), event.as_str()), ("$m1", "$m1"));
        // nothing new in the timeline
        assert_eq!(e.chat.texts().len(), 1, "the link message only");

        // delivered again: nothing more
        e.message(&room(), msg("$m1", "Anna", SAMPLE, now), now)
            .await;
        e.tick(at(2026, 11, 16, 10, 13)).await;
        assert_eq!((copies(e).len(), reactions(e), replies(e).len()), (1, 1, 1));
        let st = e.state.lock().await;
        let f = &st.minutes.plenums["house"].filings["house#2026-11-15"];
        assert_eq!(f.read.decisions.len(), 1);
        assert!(!f.pending);
    }

    #[tokio::test]
    async fn a_restart_between_saving_and_creating_makes_one_copy() {
        let mut t = setup(vec![plenum("house", "House plenum", "!house:example.org")]);
        pages_made(&t.engine).await;
        let now = at(2026, 11, 16, 10, 0);
        // the wiki fails right after the message is saved
        *t.engine.wiki.fail.lock().unwrap() = true;
        t.engine
            .message(&room(), msg("$m1", "Anna", SAMPLE, now), now)
            .await;
        assert!(copies(&t.engine).is_empty());
        assert_eq!(reactions(&t.engine), 0, "nothing before the copy");
        restart(&mut t).await;
        let e = &t.engine;
        e.tick(at(2026, 11, 16, 10, 1)).await; // still down
        *e.wiki.fail.lock().unwrap() = false;
        e.tick(at(2026, 11, 16, 10, 2)).await;
        e.tick(at(2026, 11, 16, 10, 3)).await;
        assert_eq!((copies(e).len(), reactions(e), replies(e).len()), (1, 1, 1));

        // made, then a crash before it was saved as made: found again by its ID
        let doc_id = {
            let mut st = e.state.lock().await;
            let f = st
                .minutes
                .plenums
                .get_mut("house")
                .unwrap()
                .filings
                .get_mut("house#2026-11-15")
                .unwrap();
            f.created = false;
            f.pending = true;
            f.doc_id.clone()
        };
        e.tick(at(2026, 11, 16, 10, 4)).await;
        assert_eq!(copies(e).len(), 1);
        assert!(doc_by_id(e, &doc_id).is_some());
        assert_eq!((reactions(e), replies(e).len()), (1, 1));
    }

    #[tokio::test]
    async fn the_room_failing_is_retried_and_answers_once() {
        let t = setup(vec![plenum("house", "House plenum", "!house:example.org")]);
        let e = &t.engine;
        pages_made(e).await;
        let now = at(2026, 11, 16, 10, 0);
        *e.chat.fail.lock().unwrap() = true;
        e.message(&room(), msg("$m1", "Anna", SAMPLE, now), now)
            .await;
        assert_eq!(copies(e).len(), 1, "the copy first");
        *e.chat.fail.lock().unwrap() = false;
        e.tick(at(2026, 11, 16, 10, 1)).await;
        e.tick(at(2026, 11, 16, 10, 2)).await;
        assert_eq!((copies(e).len(), reactions(e), replies(e).len()), (1, 1, 1));
    }

    #[tokio::test]
    async fn a_repost_is_refused_and_a_second_message_joins_the_copy() {
        let t = setup(vec![plenum("house", "House plenum", "!house:example.org")]);
        let e = &t.engine;
        pages_made(e).await;
        let now = at(2026, 11, 16, 10, 0);
        e.message(&room(), msg("$m1", "Anna", SAMPLE, now), now)
            .await;
        e.message(&room(), msg("$m2", "Anna", SAMPLE, now), now)
            .await;
        assert_eq!(copies(e).len(), 1);
        let r = replies(e);
        assert_eq!(r.len(), 2);
        assert!(r[1].contains("filed already"), "{}", r[1]);
        // part two, taken by Ben's !minutes as a reply
        let part2 = msg(
            "$m3",
            "Cara",
            "Teil 2\nAufgabe: Cara schreibt die Liste",
            now,
        );
        let a = take_reply(
            e,
            &person("Ben"),
            "!minutes",
            part2,
            at(2026, 11, 16, 11, 0),
        )
        .await;
        assert_eq!(a, "", "the thread says it");
        let c = copies(e);
        assert_eq!(c.len(), 1);
        assert!(
            c[0].text.contains("(2 messages)") && c[0].text.contains("Teil 2"),
            "{}",
            c[0].text
        );
        let st = e.state.lock().await;
        let f = &st.minutes.plenums["house"].filings["house#2026-11-15"];
        assert_eq!(f.messages[1].sender, "@cara:example.org");
        assert_eq!(f.messages[1].taken_by.as_deref(), Some("@ben:example.org"));
        assert_eq!(f.read.tasks.len(), 2);
    }

    #[tokio::test]
    async fn minutes_without_a_meeting_are_filed_by_their_day() {
        let t = setup(vec![plenum("house", "House plenum", "!house:example.org")]);
        let e = &t.engine;
        pages_made(e).await;
        let now = at(2026, 11, 22, 18, 0);
        let body = "Minutes 20.11.\nA spontaneous meeting\nin the garden\nabout the party";
        e.message(&room(), msg("$m1", "Anna", body, now), now).await;
        let c = copies(e);
        assert_eq!(c[0].title, "House plenum 2026-11-20: Chat minutes");
        let minutes = e.wiki.by_title("House plenum: Minutes").unwrap();
        assert_eq!(c[0].parent_id.as_deref(), Some(minutes.id.as_str()));
        assert!(
            replies(e)[0].contains("by their day, Friday 20 November 2026"),
            "{}",
            replies(e)[0]
        );
        assert!(e.state.lock().await.minutes.plenums["house"]
            .filings
            .contains_key("day:2026-11-20"));
    }

    #[tokio::test]
    async fn undo_is_for_the_sender_the_taker_the_organizers_and_the_admins() {
        let mut p = plenum("house", "House plenum", "!house:example.org");
        p.organizers = HashSet::from(["@orga:example.org".try_into().unwrap()]);
        let t = setup(vec![p]);
        let e = &t.engine;
        pages_made(e).await;
        let now = at(2026, 11, 16, 10, 0);
        let m1 = msg("$m1", "Anna", SAMPLE, now);
        e.message(&room(), m1.clone(), now).await;
        let later = at(2026, 11, 16, 10, 5);
        assert!(
            take_reply(e, &person("Ben"), "!minutes undo", m1.clone(), later)
                .await
                .starts_with("⛔")
        );
        for who in ["Anna", "Orga", "Admin"] {
            let a = take_reply(e, &person(who), "!minutes undo", m1.clone(), later).await;
            assert!(a.starts_with("↩️"), "{who}: {a}");
            assert!(copies(e).is_empty(), "the copy into the trash");
            // taken again with !minutes as a reply
            assert_eq!(
                take_reply(e, &person("Ben"), "!minutes", m1.clone(), later).await,
                ""
            );
            assert_eq!(copies(e).len(), 1);
        }
        // Ben took it last: he may withdraw it, by his latest without a reply too
        let cmd = crate::commands::parse_for("!minutes undo", &[]).unwrap();
        let a = e
            .answer(
                &room(),
                &person("Ben"),
                cmd,
                Origin::default(),
                later,
                Scope::Room,
            )
            .await;
        assert!(a[0].starts_with("↩️"), "{a:?}");
        assert_eq!(e.wiki.deleted.lock().unwrap().len(), 4);
        // each undo took the bot's mark off the message, each take put a new one on
        assert_eq!(e.chat.unreacted.lock().unwrap().len(), 4);
        assert_eq!(reactions(e), 4);
        assert!(e.chat.shown("$re-0").unwrap().starts_with("↩️ Withdrawn"));
    }

    #[tokio::test]
    async fn switched_off_nothing_is_taken_and_the_commands_say_so() {
        let mut t = setup(vec![plenum("house", "House plenum", "!house:example.org")]);
        t.engine.minutes.from_chat = false;
        let e = &t.engine;
        pages_made(e).await;
        let now = at(2026, 11, 16, 10, 0);
        e.message(&room(), msg("$m1", "Anna", SAMPLE, now), now)
            .await;
        let a = take_reply(
            e,
            &person("Ben"),
            "!minutes",
            msg("$m1", "Anna", SAMPLE, now),
            now,
        )
        .await;
        assert!(a.contains("not switched on"), "{a}");
        let a = take_reply(
            e,
            &person("Anna"),
            "!minutes undo",
            msg("$m1", "Anna", SAMPLE, now),
            now,
        )
        .await;
        assert!(a.contains("not switched on"), "{a}");
        assert!(copies(e).is_empty() && replies(e).is_empty() && reactions(e) == 0);
        // !minutes alone is the overview, as before
        let cmd = crate::commands::parse_for("!minutes", &[]).unwrap();
        let a = e
            .answer(
                &room(),
                &person("Anna"),
                cmd,
                Origin::default(),
                now,
                Scope::Room,
            )
            .await;
        assert!(a[0].starts_with("📋 **House plenum**"), "{a:?}");
    }

    #[tokio::test]
    async fn minutes_and_a_line_break_after_the_command() {
        let t = setup(vec![plenum("house", "House plenum", "!house:example.org")]);
        let e = &t.engine;
        pages_made(e).await;
        let now = at(2026, 11, 16, 10, 0);
        let body = format!("!minutes\n{SAMPLE}");
        let Some(Command::Take { key, when, text }) = crate::commands::parse_for(&body, &[]) else {
            panic!()
        };
        let mut this = msg("$c1", "Anna", "", now);
        this.body = text.clone().unwrap();
        let origin = Origin {
            this: Some(this),
            replied: None,
        };
        let cmd = Command::Take { key, when, text };
        let a = e
            .answer(&room(), &person("Anna"), cmd, origin, now, Scope::Room)
            .await;
        assert!(a.is_empty(), "{a:?}");
        assert_eq!(copies(e).len(), 1);
        assert!(copies(e)[0].text.contains("Protokoll Hausplenum 15.11."));
        assert!(!copies(e)[0].text.contains("!minutes"));
    }

    #[tokio::test]
    async fn several_plenums_in_a_room_go_by_the_first_line() {
        let t = setup(vec![
            plenum("house", "House plenum", "!house:example.org"),
            plenum("garden", "Garden group", "!house:example.org"),
        ]);
        let e = &t.engine;
        pages_made(e).await;
        let now = at(2026, 11, 16, 10, 0);
        e.message(&room(), msg("$m1", "Anna", SAMPLE, now), now)
            .await;
        assert!(copies(e).is_empty());
        assert!(replies(e)[0].contains("which plenum"), "{:?}", replies(e));
        let garden = SAMPLE.replace("Hausplenum", "Garden group");
        e.message(&room(), msg("$m2", "Anna", &garden, now), now)
            .await;
        assert_eq!(copies(e)[0].title, "Garden group 2026-11-15: Chat minutes");
        let a = take_reply(
            e,
            &person("Ben"),
            "!minutes house",
            msg("$m1", "Anna", SAMPLE, now),
            now,
        )
        .await;
        assert_eq!(a, "");
        assert_eq!(copies(e).len(), 2);
    }

    #[tokio::test]
    async fn the_bot_and_keep_accounts_are_never_taken_by_themselves() {
        let mut t = setup(vec![plenum("house", "House plenum", "!house:example.org")]);
        t.engine.me = Some("@plenum-bot:example.org".try_into().unwrap());
        t.engine.keep = HashSet::from(["@calendar:example.org".try_into().unwrap()]);
        let e = &t.engine;
        pages_made(e).await;
        let now = at(2026, 11, 16, 10, 0);
        e.message(&room(), msg("$m1", "Plenum-bot", SAMPLE, now), now)
            .await;
        e.message(&room(), msg("$m2", "Calendar", SAMPLE, now), now)
            .await;
        assert!(copies(e).is_empty() && replies(e).is_empty());
        // someone may still file a keep account's message
        let a = take_reply(
            e,
            &person("Anna"),
            "!minutes",
            msg("$m2", "Calendar", SAMPLE, now),
            now,
        )
        .await;
        assert_eq!(a, "");
        assert_eq!(copies(e).len(), 1);
    }

    fn edit_copy(e: &Engine<FakeWiki, FakeChat>, f: impl FnOnce(&mut String)) {
        let mut docs = e.wiki.docs.lock().unwrap();
        let (d, _) = (docs.iter_mut())
            .find(|(d, del)| !del && d.title.ends_with(": Chat minutes"))
            .unwrap();
        f(&mut d.text);
        d.untouched = false;
    }

    #[tokio::test]
    async fn an_edit_is_followed_and_only_the_senders_newer_ones() {
        let t = setup(vec![plenum("house", "House plenum", "!house:example.org")]);
        let e = &t.engine;
        pages_made(e).await;
        let posted = at(2026, 11, 16, 10, 12);
        e.message(&room(), msg("$m1", "Anna", SAMPLE, posted), posted)
            .await;
        let anna = person("Anna").id;
        let fixed = SAMPLE.replace("Werkzeugregal", "Werkzeugregal und Leiter");
        let edited = at(2026, 11, 16, 10, 40);
        e.edited(&room(), "$m1", &anna, "Anna", &fixed, edited, edited)
            .await;
        let c = copies(e);
        assert_eq!(c.len(), 1);
        assert!(
            c[0].text.contains("Werkzeugregal und Leiter"),
            "{}",
            c[0].text
        );
        assert!(c[0].text.contains("(last change 10:40)"));
        // the reply edited, nothing new
        assert_eq!(replies(e).len(), 1);
        assert_eq!(e.chat.edits.lock().unwrap().len(), 1);
        assert!(e
            .chat
            .shown("$re-0")
            .unwrap()
            .contains("Postponed (1): Werkzeugregal und Leiter"));
        assert!(e.chat.pinged.lock().unwrap().is_empty());
        let writes = *e.wiki.writes.lock().unwrap();

        // another account's edit, and an older one: nothing
        let ben = person("Ben").id;
        e.edited(
            &room(),
            "$m1",
            &ben,
            "Ben",
            "Protokoll\nweg\nweg\nweg",
            at(2026, 11, 16, 11, 0),
            edited,
        )
        .await;
        e.edited(
            &room(),
            "$m1",
            &anna,
            "Anna",
            SAMPLE,
            at(2026, 11, 16, 10, 30),
            edited,
        )
        .await;
        assert!(copies(e)[0].text.contains("und Leiter"));
        assert_eq!(*e.wiki.writes.lock().unwrap(), writes);
        assert_eq!(e.chat.edits.lock().unwrap().len(), 1);
        // an edit that drops the keyword keeps it taken
        let later = at(2026, 11, 16, 12, 0);
        e.edited(
            &room(),
            "$m1",
            &anna,
            "Anna",
            "Hausplenum 15.11.\nBeschluss: anders",
            later,
            later,
        )
        .await;
        assert!(copies(e)[0].text.contains("Beschluss: anders"));
    }

    #[tokio::test]
    async fn an_edit_that_makes_minutes_takes_the_message() {
        let t = setup(vec![plenum("house", "House plenum", "!house:example.org")]);
        let e = &t.engine;
        pages_made(e).await;
        let now = at(2026, 11, 16, 10, 0);
        e.message(&room(), msg("$m1", "Anna", "Hausplenum gestern", now), now)
            .await;
        assert!(copies(e).is_empty());
        let later = at(2026, 11, 16, 10, 5);
        e.edited(
            &room(),
            "$m1",
            &person("Anna").id,
            "Anna",
            SAMPLE,
            later,
            later,
        )
        .await;
        assert_eq!(copies(e).len(), 1);
        assert_eq!(
            e.chat.reactions.lock().unwrap()[0].1,
            "$m1",
            "the original's ID"
        );
    }

    #[tokio::test]
    async fn a_copy_changed_in_the_wiki_is_left_alone_and_said_once() {
        let t = setup(vec![plenum("house", "House plenum", "!house:example.org")]);
        let e = &t.engine;
        pages_made(e).await;
        let now = at(2026, 11, 16, 10, 0);
        e.message(&room(), msg("$m1", "Anna", SAMPLE, now), now)
            .await;
        edit_copy(e, |t| t.push_str("\n\nAdded in the wiki by Ben."));
        let anna = person("Anna").id;
        for (i, min) in [5, 6].into_iter().enumerate() {
            let at = at(2026, 11, 16, 10, min);
            let body = format!("{SAMPLE}\nNachtrag {i}");
            e.edited(&room(), "$m1", &anna, "Anna", &body, at, at).await;
        }
        let c = copies(e);
        assert!(c[0].text.contains("Added in the wiki by Ben.") && !c[0].text.contains("Nachtrag"));
        let r = replies(e);
        assert_eq!(r.len(), 2, "{r:?}");
        assert!(r[1].contains("changed there"), "{}", r[1]);
    }

    #[tokio::test]
    async fn a_copy_deleted_in_the_wiki_is_not_made_again() {
        let t = setup(vec![plenum("house", "House plenum", "!house:example.org")]);
        let e = &t.engine;
        pages_made(e).await;
        let now = at(2026, 11, 16, 10, 0);
        e.message(&room(), msg("$m1", "Anna", SAMPLE, now), now)
            .await;
        {
            let mut docs = e.wiki.docs.lock().unwrap();
            for (d, del) in docs.iter_mut() {
                if d.title.ends_with(": Chat minutes") {
                    *del = true;
                }
            }
        }
        let later = at(2026, 11, 16, 10, 5);
        let body = format!("{SAMPLE}\nNachtrag");
        e.edited(
            &room(),
            "$m1",
            &person("Anna").id,
            "Anna",
            &body,
            later,
            later,
        )
        .await;
        e.tick(at(2026, 11, 16, 10, 6)).await;
        assert!(copies(e).is_empty());
        assert!(e.state.lock().await.minutes.plenums["house"].filings["house#2026-11-15"].adopted);
    }

    #[tokio::test]
    async fn a_copy_archived_in_the_wiki_is_left_alone_and_fails_no_tick() {
        let t = setup(vec![plenum("house", "House plenum", "!house:example.org")]);
        let e = &t.engine;
        pages_made(e).await;
        let now = at(2026, 11, 15, 22, 0);
        e.message(&room(), msg("$m1", "Anna", SAMPLE, now), now)
            .await;
        let id = copies(e)[0].id.clone();
        {
            let mut docs = e.wiki.docs.lock().unwrap();
            let (d, _) = docs.iter_mut().find(|(d, _)| d.id == id).unwrap();
            d.archived = true;
        }
        let anna = person("Anna").id;
        let at1 = at(2026, 11, 15, 22, 30);
        let edited = SAMPLE.replace("im Januar", "im Februar");
        e.edited(&room(), "$m1", &anna, "Anna", &edited, at1, at1)
            .await;
        let report = e.tick(at(2026, 11, 15, 22, 31)).await;
        assert!(report.failed.is_empty(), "{:?}", report.failed);
        assert!(report.unavailable.is_empty(), "{:?}", report.unavailable);
        // not written (Outline would refuse it with a 403 at every tick)
        assert!(doc_by_id(e, &id).unwrap().text.contains("im Januar"));
        let st = e.state.lock().await;
        let f = &st.minutes.plenums["house"].filings[NOV15];
        assert!(f.adopted && !f.pending);
        drop(st);
        assert!(replies(e).iter().any(|r| r == ADOPTED), "{:?}", replies(e));
    }

    #[tokio::test]
    async fn a_deleted_message_is_withdrawn() {
        let t = setup(vec![plenum("house", "House plenum", "!house:example.org")]);
        let e = &t.engine;
        pages_made(e).await;
        let now = at(2026, 11, 16, 10, 0);
        e.message(&room(), msg("$m1", "Anna", SAMPLE, now), now)
            .await;
        let part2 = "Protokoll Teil 2\nAufgabe: Ben fragt\nx\ny";
        e.message(&room(), msg("$m2", "Ben", part2, now), now).await;
        assert_eq!(copies(e).len(), 1);
        e.redacted(&room(), "$m2", now).await;
        let c = copies(e);
        assert_eq!(c.len(), 1, "still there for the first message");
        assert!(!c[0].text.contains("Teil 2"), "{}", c[0].text);
        assert!(e.chat.shown("$re-1").unwrap().starts_with("↩️ Withdrawn"));
        e.redacted(&room(), "$m1", now).await;
        assert!(copies(e).is_empty(), "into the trash");
        assert_eq!(e.wiki.deleted.lock().unwrap().len(), 1);
        assert!(e.chat.shown("$re-0").unwrap().starts_with("↩️ Withdrawn"));
        // a deletion of something else changes nothing
        e.redacted(&room(), "$other", now).await;
        assert_eq!(replies(e).len(), 2);
    }

    fn closed(e: &Engine<FakeWiki, FakeChat>, id: &str) -> bool {
        let st = e.state.try_lock().unwrap();
        st.plenums["house"].meetings[id].close.done
    }

    fn edit_page(e: &Engine<FakeWiki, FakeChat>, title: &str, f: impl FnOnce(&mut String)) {
        let mut docs = e.wiki.docs.lock().unwrap();
        let (d, _) = docs
            .iter_mut()
            .find(|(d, del)| !del && d.title == title)
            .unwrap();
        f(&mut d.text);
        d.untouched = false; // an edit in Outline sets updatedAt
    }

    async fn top(e: &Engine<FakeWiki, FakeChat>, text: &str, now: DateTime<Utc>) {
        let cmd = crate::commands::parse_for(&format!("!top {text}"), &[]).unwrap();
        e.command(&room(), &person("Anna"), cmd, now).await;
    }

    const NOV15: &str = "house#2026-11-15";

    #[tokio::test]
    async fn an_empty_page_waits_for_minutes_with_one_nudge_then_closes_as_before() {
        for quiet in [false, true] {
            let mut p = plenum("house", "House plenum", "!house:example.org");
            if quiet {
                p.messages = Messages::Quiet;
            }
            let t = setup(vec![p]);
            let e = &t.engine;
            pages_made(e).await;
            e.tick(at(2026, 11, 16, 9, 0)).await; // 12 hours after the end
            e.tick(at(2026, 11, 16, 9, 1)).await;
            assert!(!closed(e, NOV15), "waits");
            let reads = *e.wiki.writes.lock().unwrap();
            e.tick(at(2026, 11, 17, 20, 59)).await;
            let nudges = |e: &Engine<FakeWiki, FakeChat>| {
                e.chat
                    .kinds()
                    .iter()
                    .filter(|k| **k == Notice::MinutesMissing)
                    .count()
            };
            assert_eq!(nudges(e), 0);
            e.tick(at(2026, 11, 17, 21, 0)).await; // two days after the end
            e.tick(at(2026, 11, 18, 21, 0)).await;
            assert_eq!(nudges(e), if quiet { 0 } else { 1 }, "quiet {quiet}");
            if !quiet {
                let text = e.chat.texts().last().unwrap().clone();
                assert!(text.starts_with("📝 No minutes for **House plenum** on Sunday, 15 November 2026 at 19:00 yet"), "{text}");
                assert!(text.contains("https://wiki.test/doc/house-plenum-2026-11-15"));
            }
            assert_eq!(
                *e.wiki.writes.lock().unwrap(),
                reads,
                "waiting writes nothing"
            );
            e.tick(at(2026, 11, 22, 20, 59)).await;
            assert!(!closed(e, NOV15));
            e.tick(at(2026, 11, 22, 21, 0)).await; // seven days: the result box
            e.tick(at(2026, 11, 22, 21, 1)).await; // closed empty, as before
            assert!(closed(e, NOV15));
            if !quiet {
                // full: the (empty) page as the minutes message
                let last = e.chat.texts().last().unwrap().clone();
                assert!(last.starts_with("📋 **Minutes of House plenum**"), "{last}");
            }
        }
    }

    #[tokio::test]
    async fn without_waiting_and_with_a_result_it_closes_at_the_usual_time() {
        let mut t = setup(vec![plenum("house", "House plenum", "!house:example.org")]);
        t.engine.minutes.wait_days = 0;
        pages_made(&t.engine).await;
        t.engine.tick(at(2026, 11, 16, 9, 0)).await;
        t.engine.tick(at(2026, 11, 16, 9, 1)).await;
        assert!(closed(&t.engine, NOV15), "wait_days 0: as before");

        let t = setup(vec![plenum("house", "House plenum", "!house:example.org")]);
        let e = &t.engine;
        pages_made(e).await;
        edit_page(e, "House plenum 2026-11-15", |t| {
            t.push_str("\nDecision: Kompost kommt.\n")
        });
        e.tick(at(2026, 11, 16, 9, 0)).await;
        e.tick(at(2026, 11, 16, 9, 1)).await;
        assert!(closed(e, NOV15), "a page with a result");
    }

    #[tokio::test]
    async fn minutes_after_the_end_close_the_meeting_with_them() {
        let t = setup(vec![plenum("house", "House plenum", "!house:example.org")]);
        let e = &t.engine;
        pages_made(e).await;
        top(e, "Werkzeugregal", at(2026, 11, 13, 10, 0)).await;
        top(e, "Garten", at(2026, 11, 13, 10, 0)).await;
        edit_page(e, "House plenum 2026-11-15", |t| {
            t.push_str("\n- [ ] Ben: Tonne kaufen\n")
        });
        let texts_before = e.chat.texts().len();
        let now = at(2026, 11, 15, 22, 0); // after the end, before the closing time
        let body = format!("{SAMPLE}\nVertagt: Kellerlicht");
        e.message(&room(), msg("$m1", "Anna", &body, now), now)
            .await;
        assert!(!closed(e, NOV15));
        e.tick(at(2026, 11, 15, 22, 1)).await; // the result box
        e.tick(at(2026, 11, 15, 22, 2)).await; // closed
        assert!(closed(e, NOV15));
        let page = e.wiki.by_title("House plenum 2026-11-15").unwrap();
        let copy = &copies(e)[0];
        assert!(
            page.text.contains(&format!(
                "📥 Chat minutes: [House plenum 2026-11-15: Chat minutes](https://wiki.test{})",
                copy.url
            )),
            "{}",
            page.text
        );
        assert!(
            page.text.contains("📝 In the chat minutes: Garten"),
            "{}",
            page.text
        );
        assert!(
            page.text.contains(
                "⏭️ On the next page: Werkzeugregal (postponed), Kellerlicht (postponed)"
            ),
            "{}",
            page.text
        );
        assert!(
            page.text.contains("✅ Decided: Räder ohne Schild"),
            "{}",
            page.text
        );
        let decisions = e.wiki.by_title("House plenum: Decisions").unwrap();
        assert!(
            decisions
                .text
                .contains("* Räder ohne Schild kommen im Januar in die Werkstatt."),
            "{}",
            decisions.text
        );
        {
            let st = e.state.lock().await;
            let ps = &st.plenums["house"];
            let tasks: Vec<&str> = ps.open_tasks.iter().map(|t| t.text.as_str()).collect();
            assert_eq!(
                tasks,
                ["Ben: Tonne kaufen", "Anna besorgt Schilder bis 30.11."]
            );
            let headings: Vec<&str> = ps.open_topics.iter().map(|t| t.heading.as_str()).collect();
            assert_eq!(headings, ["### Werkzeugregal", "### Kellerlicht"]);
        }
        // nothing more in the room: the minutes are there already
        assert_eq!(e.chat.texts().len(), texts_before);
        assert!(!e.chat.kinds().contains(&Notice::Summary));
        // the next page has only the postponed topics
        e.tick(at(2026, 11, 28, 19, 0)).await;
        let next = e.wiki.by_title("House plenum 2026-12-01").unwrap();
        assert!(
            next.text.contains("### Werkzeugregal") && next.text.contains("### Kellerlicht"),
            "{}",
            next.text
        );
        assert!(!next.text.contains("### Garten"));
        assert!(next.text.contains("Anna besorgt Schilder"));
    }

    #[test]
    fn the_pdf_is_the_page_then_the_copy() {
        assert_eq!(
            pdf_source("page", "copy"),
            "page\n\n---\n\n## Chat minutes\n\ncopy"
        );
        assert_eq!(pdf_source("page", " "), "page");
    }

    #[tokio::test]
    async fn late_minutes_and_their_edits_reach_the_lists_after_an_empty_close() {
        let mut t = setup(vec![plenum("house", "House plenum", "!house:example.org")]);
        t.engine.minutes.wait_days = 0;
        let e = &t.engine;
        pages_made(e).await;
        e.tick(at(2026, 11, 16, 9, 0)).await;
        e.tick(at(2026, 11, 16, 9, 1)).await;
        assert!(closed(e, NOV15));
        let now = at(2026, 11, 16, 12, 0);
        e.message(&room(), msg("$m1", "Anna", SAMPLE, now), now)
            .await;
        e.tick(at(2026, 11, 16, 12, 1)).await;
        let decisions = |e: &Engine<FakeWiki, FakeChat>| {
            e.wiki.by_title("House plenum: Decisions").unwrap().text
        };
        let d = decisions(e);
        assert!(d.contains("### [House plenum 2026-11-15: Chat minutes](https://wiki.test/doc/house-plenum-2026-11-15)\n\n* Räder ohne Schild"), "{d}");
        let page = e.wiki.by_title("House plenum 2026-11-15").unwrap();
        assert_eq!(
            page.text.matches("📥 Chat minutes:").count(),
            1,
            "{}",
            page.text
        );
        {
            let st = e.state.lock().await;
            let ps = &st.plenums["house"];
            assert!(ps
                .open_tasks
                .iter()
                .any(|t| t.text == "Anna besorgt Schilder bis 30.11."));
            assert!(ps
                .decisions
                .iter()
                .any(|d| d.text.starts_with("Räder ohne Schild")));
            assert_eq!(
                ps.open_topics.len(),
                1,
                "Werkzeugregal waits for the next page"
            );
        }
        let anna = person("Anna").id;
        // an edit that keeps the decisions: nothing in the list
        let same = SAMPLE.replace("Anwesend: Anna, Ben, Cara", "Anwesend: Anna, Ben");
        let at1 = at(2026, 11, 16, 13, 0);
        e.edited(&room(), "$m1", &anna, "Anna", &same, at1, at1)
            .await;
        assert_eq!(decisions(e), d);
        // one that changes them: one "(updated …)" block
        let other = same.replace("im Januar", "im Februar");
        let at2 = at(2026, 11, 16, 14, 0);
        e.edited(&room(), "$m1", &anna, "Anna", &other, at2, at2)
            .await;
        e.tick(at(2026, 11, 16, 14, 1)).await;
        let d2 = decisions(e);
        assert_eq!(
            d2.matches("(updated 16 Nov 2026, 14:00)").count(),
            1,
            "{d2}"
        );
        assert!(d2.contains("* Räder ohne Schild kommen im Februar"));
        let page = e.wiki.by_title("House plenum 2026-11-15").unwrap();
        assert_eq!(page.text.matches("📥 Chat minutes:").count(), 1);
        {
            let st = e.state.lock().await;
            let ps = &st.plenums["house"];
            let shown: Vec<&str> = ps.decisions.iter().map(|d| d.text.as_str()).collect();
            assert_eq!(
                shown,
                ["Räder ohne Schild kommen im Februar in die Werkstatt."]
            );
            assert_eq!(ps.open_topics.len(), 1);
        }
        // a task closed meanwhile (on a later page) does not come back when the minutes are
        // filed again (a retry, a rename)
        {
            let mut st = e.state.lock().await;
            st.plenums.get_mut("house").unwrap().open_tasks.clear();
            let f = st
                .minutes
                .plenums
                .get_mut("house")
                .unwrap()
                .filings
                .get_mut(NOV15)
                .unwrap();
            f.pending = true;
        }
        e.tick(at(2026, 11, 16, 14, 30)).await;
        assert!(e.state.lock().await.plenums["house"].open_tasks.is_empty());
        // withdrawn: the decisions and tasks leave again, the list says so
        e.redacted(&room(), "$m1", at(2026, 11, 16, 15, 0)).await;
        let st = e.state.lock().await;
        let ps = &st.plenums["house"];
        assert!(ps.decisions.is_empty() && ps.open_tasks.is_empty());
        drop(st);
        assert!(decisions(e).contains("(withdrawn 16 Nov 2026, 15:00)"));
    }

    #[tokio::test]
    async fn a_done_line_closes_a_task_for_good() {
        let t = setup(vec![plenum("house", "House plenum", "!house:example.org")]);
        let e = &t.engine;
        pages_made(e).await;
        edit_page(e, "House plenum 2026-11-15", |t| {
            t.push_str("\n- [ ] Ben: Tonne kaufen\n- [ ] Cara: Liste\n")
        });
        let now = at(2026, 11, 15, 22, 0);
        let body = "Protokoll 15.11.\nAnwesend: alle\n- [x] Ben: Tonne kaufen\nNichts weiter.";
        e.message(&room(), msg("$m1", "Anna", body, now), now).await;
        e.tick(at(2026, 11, 15, 22, 1)).await;
        e.tick(at(2026, 11, 15, 22, 2)).await;
        assert!(closed(e, NOV15));
        {
            let st = e.state.lock().await;
            let ps = &st.plenums["house"];
            let open: Vec<&str> = ps.open_tasks.iter().map(|t| t.text.as_str()).collect();
            assert_eq!(open, ["Cara: Liste"]);
            assert_eq!(ps.done_tasks, ["Ben: Tonne kaufen"]);
        }
        // the next page still has it unticked (copied before): it stays closed
        e.tick(at(2026, 11, 28, 19, 0)).await;
        edit_page(e, "House plenum 2026-12-01", |t| {
            t.push_str("\n- [ ] Ben: Tonne kaufen (since 15 Nov 2026)\nDecision: Weiter so.\n")
        });
        e.tick(at(2026, 12, 2, 9, 0)).await;
        e.tick(at(2026, 12, 2, 9, 1)).await;
        assert!(closed(e, "house#2026-12-01"));
        let st = e.state.lock().await;
        let open: Vec<&str> = st.plenums["house"]
            .open_tasks
            .iter()
            .map(|t| t.text.as_str())
            .collect();
        assert!(!open.iter().any(|t| t.contains("Tonne")), "{open:?}");
        assert!(open.iter().any(|t| t.contains("Cara: Liste")), "{open:?}");
    }

    fn open_tasks(st: &State) -> Vec<String> {
        (st.plenums["house"].open_tasks.iter())
            .map(|t| t.text.clone())
            .collect()
    }

    #[tokio::test]
    async fn tasks_of_late_minutes_go_onto_the_open_next_page_and_outlive_its_close() {
        let mut p = plenum("house", "House plenum", "!house:example.org");
        p.page_after_days = Some(1);
        let mut t = setup(vec![p]);
        t.engine.minutes.wait_days = 0;
        let e = &t.engine;
        pages_made(e).await;
        e.tick(at(2026, 11, 16, 9, 0)).await;
        e.tick(at(2026, 11, 16, 9, 1)).await;
        assert!(closed(e, NOV15));
        // the next page opens the day after, before any minutes came
        e.tick(at(2026, 11, 17, 0, 5)).await;
        let next = || e.wiki.by_title("House plenum 2026-12-01").unwrap().text;
        assert!(!next().contains("Anna besorgt Schilder"));
        let now = at(2026, 11, 17, 12, 0);
        e.message(&room(), msg("$m1", "Anna", SAMPLE, now), now)
            .await;
        let line = "- [ ] Anna besorgt Schilder bis 30.11. (since 15 Nov 2026)";
        assert_eq!(next().matches(line).count(), 1, "{}", next());
        // filed again (an edit that keeps the task): not a second time
        let edited = SAMPLE.replace("Anna, Ben, Cara", "Anna, Ben");
        let anna = person("Anna").id;
        let at1 = at(2026, 11, 17, 13, 0);
        e.edited(&room(), "$m1", &anna, "Anna", &edited, at1, at1)
            .await;
        assert_eq!(next().matches(line).count(), 1, "{}", next());
        // an edit that adds a task: that one too
        let more = format!("{edited}\nAufgabe: Ben fragt den Vermieter");
        let at2 = at(2026, 11, 17, 14, 0);
        e.edited(&room(), "$m1", &anna, "Anna", &more, at2, at2)
            .await;
        assert!(next().contains("- [ ] Ben fragt den Vermieter (since 15 Nov 2026)"));
        // the next meeting closes with its page as it is: both are still open
        for minute in 0..3 {
            e.tick(at(2026, 12, 2, 10, minute)).await;
        }
        assert!(closed(e, "house#2026-12-01"));
        let open = open_tasks(&*e.state.lock().await);
        assert!(open.iter().any(|t| t.contains("Anna besorgt Schilder")));
        assert!(open.iter().any(|t| t.contains("Ben fragt den Vermieter")));
        assert_eq!(open.len(), 2, "{open:?}");
    }

    #[tokio::test]
    async fn tasks_of_minutes_filed_by_a_later_day_stay_when_an_earlier_meeting_closes() {
        let t = setup(vec![plenum("house", "House plenum", "!house:example.org")]);
        let e = &t.engine;
        pages_made(e).await;
        // the meeting of the 15th waits for its minutes; a spontaneous meeting on the 17th
        let body = "Minutes 17.11.\nPresent: Anna, Ben\nIn the yard.\nTask: Ben mends the gate";
        let now = at(2026, 11, 17, 20, 0);
        e.message(&room(), msg("$m1", "Anna", body, now), now).await;
        let st = e.state.lock().await;
        assert!(st.minutes.plenums["house"]
            .filings
            .contains_key("day:2026-11-17"));
        assert_eq!(open_tasks(&st), ["Ben mends the gate"]);
        drop(st);
        // the wait is over: the earlier meeting closes empty and keeps the later task
        e.tick(at(2026, 11, 23, 9, 0)).await;
        e.tick(at(2026, 11, 23, 9, 1)).await;
        assert!(closed(e, NOV15));
        assert_eq!(open_tasks(&*e.state.lock().await), ["Ben mends the gate"]);
    }

    #[tokio::test]
    async fn a_name_as_a_done_line_closes_no_task() {
        let t = setup(vec![plenum("house", "House plenum", "!house:example.org")]);
        let e = &t.engine;
        pages_made(e).await;
        {
            let mut st = e.state.lock().await;
            let ps = st.plenums.get_mut("house").unwrap();
            for text in ["Anna: order the name tags", "Anna: call the landlord"] {
                ps.open_tasks.push(Carried {
                    text: text.into(),
                    date: chrono::NaiveDate::from_ymd_opt(2026, 11, 1).unwrap(),
                    link: String::new(),
                });
            }
        }
        let body = "Minutes 13.11.\nPresent: Anna\nIn the yard.\nNothing else.\nDone: Anna";
        let now = at(2026, 11, 13, 20, 0);
        e.message(&room(), msg("$m1", "Ben", body, now), now).await;
        assert_eq!(open_tasks(&*e.state.lock().await).len(), 2);
        // the task's own words close it, and only it
        let named = body.replace("Done: Anna", "Done: Anna: call the landlord");
        let ben = person("Ben").id;
        let at1 = at(2026, 11, 13, 21, 0);
        e.edited(&room(), "$m1", &ben, "Ben", &named, at1, at1)
            .await;
        assert_eq!(
            open_tasks(&*e.state.lock().await),
            ["Anna: order the name tags"]
        );
    }

    #[tokio::test]
    async fn a_confidential_meetings_reply_has_no_wiki_link() {
        let t = setup(vec![plenum("house", "House plenum", "!house:example.org")]);
        let e = &t.engine;
        pages_made(e).await;
        let cmd = crate::commands::parse_for("!minutes confidential 15.11.", &[]).unwrap();
        let a = e
            .command(&room(), &person("Anna"), cmd, at(2026, 11, 13, 10, 0))
            .await;
        assert!(a.starts_with("🔒"), "{a}");
        let now = at(2026, 11, 16, 10, 0);
        e.message(&room(), msg("$m1", "Anna", SAMPLE, now), now)
            .await;
        let r = replies(e);
        assert!(
            r[0].starts_with("📥 Filed as the minutes of House plenum")
                && !r[0].contains("https://"),
            "{}",
            r[0]
        );
        assert_eq!(copies(e).len(), 1, "the wiki has it as always");
    }

    #[tokio::test]
    async fn the_copy_follows_its_meeting_when_it_moves() {
        let t = setup(vec![plenum("house", "House plenum", "!house:example.org")]);
        let e = &t.engine;
        pages_made(e).await;
        let now = at(2026, 11, 15, 19, 30);
        e.message(&room(), msg("$m1", "Anna", SAMPLE, now), now)
            .await;
        // moved (still to come in the calendar's eyes: a day later)
        {
            let mut st = e.state.lock().await;
            let ps = st.plenums.get_mut("house").unwrap();
            for o in ps.occurrences.iter_mut().filter(|o| o.id == NOV15) {
                o.start = "2026-11-16 19:00".parse().unwrap();
                o.end = Some(o.start.0 + Duration::hours(2));
            }
        }
        e.tick(at(2026, 11, 15, 19, 31)).await;
        e.tick(at(2026, 11, 15, 19, 32)).await;
        let c = copies(e);
        assert_eq!(c[0].title, "House plenum 2026-11-16: Chat minutes", "{c:?}");
    }

    fn ben_is_known(e: &Engine<FakeWiki, FakeChat>) {
        use mxbot_common::directory::{Choice, People, Person};
        let house: OwnedUserId = "@ben:example.org".try_into().unwrap();
        e.set_people(&People::new(vec![Person {
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
    }

    fn with_pdf(t: &mut T) -> bool {
        let font = ["/usr/share/fonts/truetype/dejavu", "/usr/share/fonts/TTF"]
            .into_iter()
            .find(|d| std::path::Path::new(d).join("DejaVuSans.ttf").exists());
        if let Some(dir) = font {
            t.engine.pdf = PdfConfig {
                enabled: true,
                font_dir: dir.to_owned(),
                post_to_room: true,
            };
        }
        font.is_some()
    }

    const ROOM: &str = "!house:example.org";

    #[tokio::test]
    async fn wiki_minutes_in_full_are_one_message_and_pinned() {
        let mut t = setup(vec![plenum("house", "House plenum", ROOM)]);
        let pdf = with_pdf(&mut t);
        let e = &t.engine;
        ben_is_known(e);
        // a pin of someone else's stays
        e.chat
            .room_pins
            .lock()
            .unwrap()
            .push((ROOM.into(), "$someones".into()));
        pages_made(e).await;
        top(e, "Bins", at(2026, 11, 13, 10, 0)).await;
        top(e, "Garden", at(2026, 11, 13, 10, 0)).await;
        edit_page(e, "House plenum 2026-11-15", |t| {
            *t = t.replacen("**Result:**", "**Result:** yellow ones\n\nDecision: We buy yellow bins.\n\n- [ ] @[Ben Berg](mention://m1/user/u1): order them", 1);
            *t = t.replacen("|  |  |  |", "| Anna | Cara | all |", 1);
        });
        e.tick(at(2026, 11, 16, 9, 0)).await;
        e.tick(at(2026, 11, 16, 9, 1)).await;
        assert!(closed(e, NOV15));
        let texts = e.chat.texts();
        let last = texts.last().unwrap();
        assert!(last.starts_with("📋 **Minutes of House plenum**, Sunday, 15 November 2026 at 19:00\n\nFacilitation: Anna · Minutes: Cara · Present: all"), "{last}");
        assert!(last.contains("**Bins**\n- yellow ones\n- Decision: We buy yellow bins.\n- ☐ [Ben Berg](https://matrix.to/#/@ben:example.org): order them"), "{last}");
        assert!(
            last.contains("**Garden** (on the next page)")
                && last.contains("**Postponed:** Garden")
        );
        assert!(last.contains("**Open tasks**"));
        assert!(last.ends_with("The page: https://wiki.test/doc/house-plenum-2026-11-15"));
        assert_eq!(
            e.chat.pinged.lock().unwrap().len(),
            1,
            "the mentions the summary had"
        );
        assert_eq!(
            e.chat
                .kinds()
                .iter()
                .filter(|k| **k == Notice::Summary)
                .count(),
            1
        );
        if pdf {
            assert!(
                e.chat.files.lock().unwrap().is_empty(),
                "the PDF only in the wiki"
            );
            assert_eq!(e.wiki.attachments.lock().unwrap().len(), 1);
        }
        let first = format!("$ev-{}", texts.len() - 1);
        assert_eq!(
            e.chat.pinned_in(ROOM),
            ["$someones".to_owned(), first.clone()]
        );

        // the next meeting's minutes take the pin; the person's stays
        e.tick(at(2026, 11, 28, 19, 0)).await;
        edit_page(e, "House plenum 2026-12-01", |t| {
            t.push_str("\nDecision: More bins.\n")
        });
        e.tick(at(2026, 12, 2, 9, 0)).await;
        e.tick(at(2026, 12, 2, 9, 1)).await;
        let second = format!("$ev-{}", e.chat.texts().len() - 1);
        assert_ne!(first, second);
        assert_eq!(e.chat.pinned_in(ROOM), ["$someones".to_owned(), second]);
    }

    #[tokio::test]
    async fn too_long_a_message_and_the_other_modes_send_what_they_sent_before() {
        // too long: the summary
        let t = setup(vec![plenum("house", "House plenum", ROOM)]);
        let e = &t.engine;
        pages_made(e).await;
        edit_page(e, "House plenum 2026-11-15", |t| {
            for i in 0..1500 {
                t.push_str(&format!("\nDecision: number {i} of many.\n"));
            }
        });
        e.tick(at(2026, 11, 16, 9, 0)).await;
        e.tick(at(2026, 11, 16, 9, 1)).await;
        let last = e.chat.texts().last().unwrap().clone();
        assert!(
            last.starts_with("✅ The minutes of **House plenum**")
                && last.contains("**Decisions**"),
            "{}",
            &last[..80]
        );
        assert!(e.chat.pinned_in(ROOM).is_empty(), "a summary is not pinned");

        for (mode, quiet) in [
            (Publication::Link, false),
            (Publication::Confidential, false),
            (Publication::Full, true),
        ] {
            let mut p = plenum("house", "House plenum", ROOM);
            p.publication = mode;
            if quiet {
                p.messages = Messages::Quiet;
            }
            let t = setup(vec![p]);
            let e = &t.engine;
            pages_made(e).await;
            edit_page(e, "House plenum 2026-11-15", |t| {
                t.push_str("\nDecision: Kompost kommt.\n")
            });
            e.tick(at(2026, 11, 16, 9, 0)).await;
            e.tick(at(2026, 11, 16, 9, 1)).await;
            assert!(closed(e, NOV15));
            let texts = e.chat.texts();
            if quiet {
                assert_eq!(texts.len(), 1, "the link message only: {texts:?}");
            } else {
                assert!(
                    texts.last().unwrap().contains("are closed"),
                    "{mode:?}: {texts:?}"
                );
                assert!(!texts.last().unwrap().contains("Kompost"));
            }
            assert!(e.chat.pinned_in(ROOM).is_empty());
        }
    }

    #[tokio::test]
    async fn chat_minutes_are_pinned_and_unpinned_when_withdrawn() {
        let t = setup(vec![plenum("house", "House plenum", ROOM)]);
        let e = &t.engine;
        e.chat
            .room_pins
            .lock()
            .unwrap()
            .push((ROOM.into(), "$someones".into()));
        pages_made(e).await;
        let now = at(2026, 11, 16, 10, 0);
        e.message(&room(), msg("$m1", "Anna", SAMPLE, now), now)
            .await;
        e.tick(at(2026, 11, 16, 10, 1)).await;
        assert_eq!(e.chat.pinned_in(ROOM), ["$someones", "$m1"]);
        let calls = *e.chat.pin_events.lock().unwrap();
        e.tick(at(2026, 11, 16, 10, 2)).await;
        assert_eq!(
            *e.chat.pin_events.lock().unwrap(),
            calls,
            "only when it changes"
        );
        e.redacted(&room(), "$m1", at(2026, 11, 16, 11, 0)).await;
        e.tick(at(2026, 11, 16, 11, 1)).await;
        assert_eq!(e.chat.pinned_in(ROOM), ["$someones"]);
    }

    #[tokio::test]
    async fn a_room_that_does_not_let_the_bot_pin_and_pin_off() {
        let t = setup(vec![plenum("house", "House plenum", ROOM)]);
        let e = &t.engine;
        *e.chat.no_room_pins.lock().unwrap() = true;
        pages_made(e).await;
        let now = at(2026, 11, 16, 10, 0);
        e.message(&room(), msg("$m1", "Anna", SAMPLE, now), now)
            .await;
        for min in 1..4 {
            e.tick(at(2026, 11, 16, 10, min)).await;
        }
        assert_eq!(*e.chat.pin_events.lock().unwrap(), 1, "tried once");
        assert_eq!(copies(e).len(), 1, "and goes on");

        let mut t = setup(vec![plenum("house", "House plenum", ROOM)]);
        t.engine.minutes.pin = false;
        let e = &t.engine;
        pages_made(e).await;
        e.message(&room(), msg("$m1", "Anna", SAMPLE, now), now)
            .await;
        e.tick(at(2026, 11, 16, 10, 1)).await;
        assert_eq!(*e.chat.pin_events.lock().unwrap(), 0);
    }

    async fn with_open_task(e: &Engine<FakeWiki, FakeChat>) {
        let mut st = e.state.lock().await;
        st.plenums.get_mut("house").unwrap().open_tasks = vec![Carried {
            text: "Ben: write to the landlord (since 1 Nov 2026)".into(),
            date: NaiveDate::from_ymd_opt(2026, 11, 1).unwrap(),
            link: String::new(),
        }];
    }

    fn templates(e: &Engine<FakeWiki, FakeChat>) -> Vec<(String, String)> {
        let kinds = e.chat.notices.lock().unwrap().clone();
        (e.chat.sent.lock().unwrap().iter())
            .enumerate()
            .filter(|(_, (_, _, txn))| {
                kinds
                    .iter()
                    .any(|(t, k)| t == txn && *k == Notice::Template)
            })
            .map(|(i, (_, text, _))| (format!("$ev-{i}"), text.clone()))
            .collect()
    }

    #[tokio::test]
    async fn the_template_of_a_full_meeting_has_its_topics_and_open_tasks() {
        let t = setup(vec![plenum("house", "House plenum", ROOM)]);
        let e = &t.engine;
        with_open_task(e).await;
        pages_made(e).await;
        top(e, "Bins", at(2026, 11, 13, 10, 0)).await;
        top(e, "Garden_party", at(2026, 11, 13, 10, 0)).await;
        e.tick(at(2026, 11, 15, 18, 30)).await; // the reminder, then the template
        let kinds = e.chat.kinds();
        assert_eq!(
            &kinds[kinds.len() - 2..],
            [Notice::Reminder, Notice::Template]
        );
        let posted = templates(e);
        assert_eq!(posted.len(), 1);
        let text = &posted[0].1;
        assert_eq!(
            text,
            "Minutes House plenum 15 November 2026\nPresent:\nFacilitation:\nMinutes by:\n\n## Bins\n\n## Garden\\_party\n\nDecisions:\n-\n\nTasks:\n-\n\nPostponed:\n-\n\nDone:\n- [ ] Ben: write to the landlord (since 1 Nov 2026)"
        );
        assert!(e.wiki.by_title("House plenum: Chat template").is_some());
        // once: not again at the next ticks, nor after a restart
        e.tick(at(2026, 11, 15, 18, 31)).await;
        e.tick(at(2026, 11, 15, 19, 0)).await;
        assert_eq!(templates(e).len(), 1);

        // posted back as it is: taken, files nothing
        let now = at(2026, 11, 15, 21, 30);
        e.message(&room(), msg("$m1", "Anna", text, now), now).await;
        {
            let st = e.state.lock().await;
            let f = &st.minutes.plenums["house"].filings[NOV15];
            assert_eq!(f.read, Read::default(), "{:?}", f.read);
        }
        assert!(replies(e)[0].contains("No decisions or tasks recognised"));
        // the template message now one line, edited once
        let edits = e.chat.edits.lock().unwrap().clone();
        let shortened: Vec<_> = edits.iter().filter(|x| x.1 == posted[0].0).collect();
        assert_eq!(shortened.len(), 1);
        assert_eq!(shortened[0].2, "📋 Template for House plenum on Sunday, 15 November 2026 at 19:00. The minutes are filed.");
        // the meeting closes with them; the template is not edited again
        e.tick(at(2026, 11, 15, 21, 31)).await;
        e.tick(at(2026, 11, 15, 21, 32)).await;
        assert!(closed(e, NOV15));
        assert_eq!(
            e.chat
                .edits
                .lock()
                .unwrap()
                .iter()
                .filter(|x| x.1 == posted[0].0)
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn a_filled_in_template_is_read_entry_by_entry() {
        let filled = "Minutes House plenum 15 November 2026\nPresent: Anna, Ben\nFacilitation: Anna\nMinutes by: Ben\n\n## Bins\nWe buy yellow ones.\n\nDecisions:\n- Yellow bins for the yard\n- A new rota\n\nTasks:\n- Ben: order the bins\n- [ ] Cara: write the rota\n\nPostponed:\n- Garden party\n\nDone:\n- [x] Ben: write to the landlord (since 1 Nov 2026)\n- [ ] Dirk: still open";
        let r = minutes::read(filled);
        assert_eq!(r.decisions, ["Yellow bins for the yard", "A new rota"]);
        assert_eq!(r.tasks, ["Ben: order the bins", "Cara: write the rota"]);
        assert_eq!(r.postponed, ["Garden party"]);
        assert_eq!(r.done, ["Ben: write to the landlord (since 1 Nov 2026)"]);
        assert!(minutes::is_minutes(filled));
    }

    #[tokio::test]
    async fn a_link_or_confidential_meetings_template_has_no_wiki_content() {
        for mode in [Publication::Link, Publication::Confidential] {
            let mut p = plenum("house", "House plenum", ROOM);
            p.publication = mode;
            let t = setup(vec![p]);
            let e = &t.engine;
            with_open_task(e).await;
            pages_made(e).await;
            top(e, "Bins", at(2026, 11, 13, 10, 0)).await;
            e.tick(at(2026, 11, 15, 18, 30)).await;
            let text = templates(e)[0].1.clone();
            assert!(
                !text.contains("Bins") && !text.contains("landlord"),
                "{mode:?}: {text}"
            );
            assert!(text.contains("Decisions:\n-\n\nTasks:"));
        }
    }

    #[tokio::test]
    async fn a_template_edited_in_the_wiki_is_used_and_its_minutes_read() {
        let t = setup(vec![plenum("house", "House plenum", ROOM)]);
        let e = &t.engine;
        pages_made(e).await;
        edit_page(e, "House plenum: Chat template", |t| {
            *t = "Unsere Vorlage:\n\n```\nProtokoll {{plenum}} {{date}}\nAnwesend:\n\nBeschlüsse:\n-\n\nAufgaben:\n-\n\nVertagt:\n-\n\nErledigt:\n{{open_tasks}}\n```\n".into();
        });
        e.tick(at(2026, 11, 15, 18, 30)).await;
        let text = templates(e)[0].1.clone();
        assert_eq!(text, "Protokoll House plenum 15 November 2026\nAnwesend:\n\nBeschlüsse:\n-\n\nAufgaben:\n-\n\nVertagt:\n-\n\nErledigt:");
        // the bot's text is not put back: someone changed it
        e.tick(at(2026, 11, 15, 20, 0)).await;
        let doc = e.wiki.by_title("House plenum: Chat template").unwrap();
        assert!(doc.text.starts_with("Unsere Vorlage"));
        let filled = text
            .replace("Beschlüsse:\n-", "Beschlüsse:\n- Gelbe Tonnen")
            .replace("Aufgaben:\n-", "Aufgaben:\n- Ben bestellt sie")
            .replace("Vertagt:\n-", "Vertagt:\n- Gartenfest");
        let now = at(2026, 11, 15, 21, 30);
        e.message(&room(), msg("$m1", "Anna", &filled, now), now)
            .await;
        let st = e.state.lock().await;
        let f = &st.minutes.plenums["house"].filings[NOV15];
        assert_eq!(f.read.decisions, ["Gelbe Tonnen"]);
        assert_eq!(f.read.tasks, ["Ben bestellt sie"]);
        assert_eq!(f.read.postponed, ["Gartenfest"]);
    }

    #[tokio::test]
    async fn the_automatic_template_not_in_quiet_nor_switched_off_nor_twice() {
        let mut p = plenum("house", "House plenum", ROOM);
        p.messages = Messages::Quiet;
        let t = setup(vec![p]);
        pages_made(&t.engine).await;
        t.engine.tick(at(2026, 11, 15, 18, 30)).await;
        assert!(templates(&t.engine).is_empty(), "quiet");

        let mut t = setup(vec![plenum("house", "House plenum", ROOM)]);
        t.engine.minutes.template = false;
        pages_made(&t.engine).await;
        t.engine.tick(at(2026, 11, 15, 18, 30)).await;
        assert!(templates(&t.engine).is_empty(), "template = false");

        let mut t = setup(vec![plenum("house", "House plenum", ROOM)]);
        pages_made(&t.engine).await;
        t.engine.tick(at(2026, 11, 15, 18, 30)).await;
        restart(&mut t).await;
        t.engine.tick(at(2026, 11, 15, 18, 31)).await;
        assert_eq!(templates(&t.engine).len(), 1, "not after a restart");
    }

    #[tokio::test]
    async fn the_template_is_shortened_at_a_close_without_minutes_and_the_commands_is_not() {
        let mut t = setup(vec![plenum("house", "House plenum", ROOM)]);
        t.engine.minutes.wait_days = 0;
        let e = &t.engine;
        pages_made(e).await;
        e.tick(at(2026, 11, 15, 18, 30)).await;
        let cmd = crate::commands::parse_for("!minutes template", &[]).unwrap();
        let a = e
            .answer(
                &room(),
                &person("Anna"),
                cmd,
                Origin::default(),
                at(2026, 11, 15, 20, 0),
                Scope::Room,
            )
            .await;
        assert!(a.is_empty(), "{a:?}");
        let posted = templates(e);
        assert_eq!(
            posted.len(),
            3,
            "the automatic one, the command's line and its template"
        );
        assert!(posted[1]
            .1
            .starts_with("📋 The template for House plenum on Sunday, 15 November 2026 at 19:00"));
        assert!(posted[2]
            .1
            .starts_with("Minutes House plenum 15 November 2026"));
        e.tick(at(2026, 11, 16, 9, 0)).await;
        e.tick(at(2026, 11, 16, 9, 1)).await;
        assert!(closed(e, NOV15));
        let edits = e.chat.edits.lock().unwrap().clone();
        assert_eq!(edits.len(), 1, "{edits:?}");
        assert_eq!(edits[0].1, posted[0].0);
        assert!(edits[0]
            .2
            .ends_with("The meeting is closed. `!minutes template` gives it again."));
    }

    #[tokio::test]
    async fn a_template_edit_that_failed_is_tried_again() {
        let t = setup(vec![plenum("house", "House plenum", ROOM)]);
        let e = &t.engine;
        pages_made(e).await;
        e.tick(at(2026, 11, 15, 18, 30)).await;
        assert_eq!(templates(e).len(), 1);
        // as after a try that the homeserver did not take: counted, the text still the template
        {
            let mut st = e.state.lock().await;
            let m = st.plenums.get_mut("house").unwrap();
            let sent = m.meetings.get_mut(NOV15).unwrap().template_message.as_mut();
            sent.unwrap().edits = 1;
        }
        let now = at(2026, 11, 15, 22, 0);
        e.message(&room(), msg("$m1", "Anna", SAMPLE, now), now)
            .await;
        let edits = e.chat.edits.lock().unwrap().clone();
        assert_eq!(edits.len(), 1, "{edits:?}");
        assert!(edits[0].2.ends_with("The minutes are filed."), "{edits:?}");
        // and then no more: not at the close either
        e.tick(at(2026, 11, 15, 22, 1)).await;
        e.tick(at(2026, 11, 15, 22, 2)).await;
        assert!(closed(e, NOV15));
        assert_eq!(e.chat.edits.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn two_changes_of_the_decisions_on_one_day_are_two_blocks() {
        let mut t = setup(vec![plenum("house", "House plenum", ROOM)]);
        t.engine.minutes.wait_days = 0;
        let e = &t.engine;
        pages_made(e).await;
        e.tick(at(2026, 11, 16, 9, 0)).await;
        e.tick(at(2026, 11, 16, 9, 1)).await;
        assert!(closed(e, NOV15));
        let now = at(2026, 11, 16, 12, 0);
        e.message(&room(), msg("$m1", "Anna", SAMPLE, now), now)
            .await;
        let anna = person("Anna").id;
        for (hour, month) in [(13, "Februar"), (14, "März")] {
            let at = at(2026, 11, 16, hour, 0);
            let body = SAMPLE.replace("Januar", month);
            e.edited(&room(), "$m1", &anna, "Anna", &body, at, at).await;
        }
        let d = e.wiki.by_title("House plenum: Decisions").unwrap().text;
        assert!(d.contains("(updated 16 Nov 2026, 13:00)") && d.contains("im Februar"));
        assert!(d.contains("(updated 16 Nov 2026, 14:00)") && d.contains("im März"));
    }

    #[test]
    fn the_help_has_the_chat_minutes() {
        let h = help();
        for cmd in [
            "`!minutes` as a reply",
            "`!minutes undo`",
            "`!minutes template`",
            "Protokoll",
        ] {
            assert!(h.contains(cmd), "{cmd}");
        }
    }

    #[tokio::test]
    async fn ten_a_day_and_no_more() {
        let t = setup(vec![plenum("house", "House plenum", "!house:example.org")]);
        let e = &t.engine;
        pages_made(e).await;
        let now = at(2026, 11, 16, 10, 0);
        for i in 0..12 {
            let body = format!("Minutes\nnumber {i}\nline\nline");
            e.message(&room(), msg(&format!("$m{i}"), "Bot", &body, now), now)
                .await;
        }
        let st = e.state.lock().await;
        assert_eq!(
            st.minutes.plenums["house"].filings["house#2026-11-15"]
                .messages
                .len(),
            10
        );
    }
}
