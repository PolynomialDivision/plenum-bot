//! The engine's part of the topics (docs/TOPICS.md, topics.rs): `!top` makes a numbered topic
//! with a message of its own, the tick places it in the minutes, keeps its message and the
//! announcement up to date and, after the meeting, says in its thread what came of it.
//!
//! What comes in from Matrix besides commands (reactions, redactions, edits, thread messages)
//! is only noted ([`Engine::note`], no lock, no network: the SDK waits for its handlers in the
//! sync); the next tick reads it. Reactions are never taken from the events alone: a topic's are
//! counted again from the homeserver when one came in, every hour, and before its outcome.

use super::*;
use crate::{
    commands::TopicAction,
    plenum_settings,
    topics::{self, Outcome, Posted, Status, Target, Topic},
};

/// Where a message goes.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Place<'a> {
    Room,
    /// A reply to this event (in the room's timeline).
    ReplyTo(&'a str),
    /// In the thread of this event.
    Thread(&'a str),
}

/// Something from Matrix the topics may care about (only from the plenums' rooms).
#[derive(Clone, Debug, PartialEq)]
pub enum Inbound {
    Reaction {
        room: OwnedRoomId,
        target: String,
    },
    Redaction {
        room: OwnedRoomId,
        redacts: String,
        sender: OwnedUserId,
    },
    /// An edit (`m.replace`) of `original`, with the new text.
    Edit {
        room: OwnedRoomId,
        original: String,
        sender: OwnedUserId,
        body: String,
    },
    /// A message in the thread of `root`.
    Thread {
        room: OwnedRoomId,
        root: String,
        event: String,
        sender: OwnedUserId,
        body: String,
    },
}

/// What a `!top` message is besides its text.
#[derive(Clone, Debug, Default)]
pub struct Origin {
    /// Its event ID (a message seen twice makes one topic; its edits and deletion count).
    pub event: Option<String>,
    /// The name of whoever wrote the message a `!top` reply answers.
    pub from: Option<String>,
    /// Their account and that message.
    pub from_user: Option<OwnedUserId>,
    pub from_event: Option<String>,
}

/// Inbound items kept at most (a reaction storm): more are dropped, the hourly count catches up.
const INBOX_MAX: usize = 5000;
/// A topic's reactions are counted again at least this often.
const RECOUNT_EVERY: Duration = Duration::hours(1);
/// Reactions the bot reads under one message at most (main.rs, `reactions`).
pub const REACTIONS_READ: usize = 1000;

/// The transaction ID of the `n`th edit of `event`: never one of an earlier edit, even when
/// the text goes back to an earlier one (the homeserver would drop it as a resend).
fn edit_txn(event: &str, n: u32) -> String {
    format!("plenum-edit-{}-{n}", event.trim_start_matches('$'))
}

/// The `!top` text of a message body: without `!top` and the plenum key.
fn top_text<'a>(body: &'a str, keys: &[&str]) -> Option<&'a str> {
    let body = body.trim_start();
    let head = body.get(..4)?;
    if !head.eq_ignore_ascii_case("!top") {
        return None;
    }
    let rest = body[4..].strip_prefix(char::is_whitespace)?.trim_start();
    let first = rest.split_whitespace().next().unwrap_or("");
    Some(if keys.contains(&first) {
        rest[first.len()..].trim_start()
    } else {
        rest
    })
}

impl<W: Wiki, C: Chat> Engine<W, C> {
    /// Something for the topics from Matrix: kept until the next tick.
    pub fn note(&self, item: Inbound) {
        let mut inbox = self.inbox.lock().unwrap();
        if inbox.len() < INBOX_MAX && !inbox.contains(&item) {
            inbox.push(item);
        }
    }

    /// The plenum with its room's settings (plenum_settings.rs).
    pub(super) fn effective(&self, base: &Plenum, st: &State) -> Plenum {
        match st.book.plenums.get(&base.key) {
            Some(area) => plenum_settings::apply(base, &area.settings),
            None => base.clone(),
        }
    }

    /// The person behind `account` in a fresh read of the directory (None: unknown, claimed
    /// twice, or no fresh read).
    fn person_of(&self, account: &UserId) -> Option<(String, String)> {
        let snapshot = self.snapshot.lock().unwrap().clone()?;
        let age = std::time::SystemTime::now()
            .duration_since(snapshot.read_at)
            .unwrap_or_default();
        if age > self.stale_after {
            return None;
        }
        snapshot
            .people
            .owner_of(account)
            .map(|p| (p.id.clone(), p.name.clone()))
    }

    /// Whether `user` is the author of `t`: their account, or another account of the same
    /// person (directory).
    fn is_author(&self, t: &Topic, user: &UserId) -> bool {
        let Some(by) = &t.by_user else {
            return false;
        };
        if by == user.as_str() {
            return true;
        }
        let Ok(by) = OwnedUserId::try_from(by.as_str()) else {
            return false;
        };
        match (self.person_of(&by), self.person_of(user)) {
            (Some((a, _)), Some((b, _))) => a == b,
            _ => false,
        }
    }

    /// The author (and whoever wrote the message a reply-`!top` brought in), the plenum's
    /// organizers and the admins act on a topic.
    fn may_act(&self, p: &Plenum, t: &Topic, user: &UserId) -> bool {
        self.is_author(t, user)
            || t.from_user.as_deref() == Some(user.as_str())
            || p.organizers.contains(user)
            || self.admins.contains(user)
    }

    /// Who may change a topic, for a refusal.
    fn who_may(&self, p: &Plenum, t: &Topic) -> String {
        let organizers = if p.organizers.is_empty() {
            String::new()
        } else {
            let mut names: Vec<String> = p
                .organizers
                .iter()
                .map(|u| u.localpart().to_owned())
                .collect();
            names.sort();
            format!(", the plenum's organizers ({})", names.join(", "))
        };
        format!(
            "⛔ Only {}, who brought in #{}, can change it{organizers} or a bot admin.",
            topics::md(&t.by),
            t.number
        )
    }

    /// The name the bot writes for `sender`: the directory's, else the display name.
    fn shown_name(&self, sender: &Sender) -> String {
        let name = self
            .person_of(&sender.id)
            .map(|(_, name)| name)
            .filter(|n| !n.trim().is_empty())
            .unwrap_or_else(|| sender.name.clone());
        topics::person_name(&name)
    }

    /// The old `pending_tops` (from before topics) become topics, once.
    pub(super) fn migrate_pending(&self, st: &mut State, p: &Plenum) {
        if !p.topics.enabled {
            return;
        }
        let pending =
            std::mem::take(&mut st.plenums.entry(p.key.clone()).or_default().pending_tops);
        let area = st.book.plenums.entry(p.key.clone()).or_default();
        if area.migrated && pending.is_empty() {
            return;
        }
        for top in pending {
            // moved already (topics.json was saved, state.json not: a crash in between)
            let by = topics::person_name(&top.by);
            if area
                .topics
                .values()
                .any(|t| t.by_user.is_none() && t.at == top.at && t.by == by)
            {
                continue;
            }
            let number = area.next_number();
            let title = topics::one_line(&top.text, topics::MAX_TITLE);
            area.topics.insert(
                number,
                Topic {
                    number,
                    title: if title.is_empty() {
                        "…".into()
                    } else {
                        title
                    },
                    by,
                    at: top.at,
                    // no message of its own: nothing to post
                    card: crate::state::Step {
                        txn: None,
                        done: true,
                    },
                    buttons: true,
                    ..Default::default()
                },
            );
        }
        area.migrated = true;
    }

    /// The meeting a topic goes to now: the one it is placed in, else the next one not closed
    /// (a running one counts) after its `later`.
    fn target_of(
        &self,
        st: &mut State,
        p: &Plenum,
        t: &Topic,
        now: DateTime<Utc>,
    ) -> Option<(String, MeetingKey)> {
        if let Some(id) = &t.placed {
            if let Some(start) = st
                .plenums
                .get(&p.key)
                .and_then(|ps| ps.meetings.get(id))
                .and_then(|m| m.start)
            {
                return Some((id.clone(), start));
            }
        }
        self.upcoming(st, p, now, 12)
            .into_iter()
            .find(|(_, start)| t.not_after.is_none_or(|after| *start > after))
    }

    fn card_target(&self, st: &mut State, p: &Plenum, t: &Topic, now: DateTime<Utc>) -> Target {
        match self.target_of(st, p, t, now) {
            Some((_, start)) => Target::Meeting(start),
            None => Target::Next,
        }
    }

    /// `!top <text>` with topics: a new topic and its message. The answer is empty when the
    /// message is the answer.
    pub(super) async fn new_topic(
        &self,
        st: &mut State,
        p: &Plenum,
        sender: &Sender,
        origin: &Origin,
        text: &str,
        now: DateTime<Utc>,
    ) -> String {
        let area = st.book.plenums.entry(p.key.clone()).or_default();
        if let Some(event) = &origin.event {
            if area.by_event(event).is_some() {
                return String::new(); // seen before (a sync after a restart)
            }
        }
        let Some(mut s) = topics::parse_submission(text) else {
            return "Please add the topic: `!top <topic>`".to_owned();
        };
        let today = area
            .topics
            .values()
            .filter(|t| t.by_user.as_deref() == Some(sender.id.as_str()))
            .filter(|t| now - t.at < Duration::days(1))
            .count();
        if today >= topics::PER_PERSON_A_DAY {
            return format!(
                "⏳ That is {} topics from you today already; more tomorrow, please.",
                topics::PER_PERSON_A_DAY
            );
        }
        let mine = area
            .open()
            .filter(|t| t.by_user.as_deref() == Some(sender.id.as_str()))
            .count();
        if mine >= topics::MAX_OPEN_PER_PERSON {
            return format!(
                "⏳ You have {} open topics in {} already; please bring this one in when some are dealt with.",
                topics::MAX_OPEN_PER_PERSON,
                p.name
            );
        }
        if area.open().count() >= topics::MAX_OPEN {
            return format!(
                "⏳ {} has {} open topics; please bring this one in when some are dealt with.",
                p.name,
                topics::MAX_OPEN
            );
        }
        // Someone else's message: its first line is the topic, nothing more of it is copied.
        if origin.from.is_some() {
            s.details.clear();
            s.proposal = None;
            s.decide = false;
        }
        let number = area.next_number();
        let mut t = Topic {
            number,
            title: s.title,
            details: s.details,
            proposal: s.proposal,
            decide: s.decide,
            by: self.shown_name(sender),
            by_user: Some(sender.id.to_string()),
            from: origin.from.as_deref().map(topics::person_name),
            from_user: origin.from_user.as_ref().map(|u| u.to_string()),
            from_event: origin.from_event.clone(),
            at: now,
            source: origin.event.clone(),
            ..Default::default()
        };
        t.card.txn();
        area.topics.insert(number, t);
        if let Err(e) = self.save(st).await {
            error!("{e:#}");
            // not saved: not there (its number is not used again)
            if let Some(area) = st.book.plenums.get_mut(&p.key) {
                area.topics.remove(&number);
            }
            return "⚠️ The topic could not be saved just now. Please try again in a moment."
                .to_owned();
        }
        info!(plenum = %p.key, topic = number, "Topic brought in");
        match self.post_card(st, p, number, now).await {
            Ok(()) => String::new(),
            Err(e) => {
                warn!(plenum = %p.key, topic = number, "The topic's message: {e:#} (again at the next tick)");
                format!("📌 #{number} is noted; its message follows in a moment.")
            }
        }
    }

    /// Posts a topic's message (once: its transaction ID is saved), with its buttons.
    async fn post_card(
        &self,
        st: &mut State,
        p: &Plenum,
        number: u32,
        now: DateTime<Utc>,
    ) -> Result<()> {
        let t = st.book.plenums[&p.key].topics[&number].clone();
        if t.card.done {
            return Ok(());
        }
        let txn = t
            .card
            .txn
            .clone()
            .context("a card without its transaction")?;
        let target = self.card_target(st, p, &t, now);
        let text = topics::card(&t, target, p.topics.tell_interested);
        let author: Vec<OwnedUserId> = t
            .by_user
            .as_deref()
            .and_then(|u| OwnedUserId::try_from(u).ok())
            .into_iter()
            .collect();
        let place = match &t.source {
            Some(source) => Place::ReplyTo(source),
            None => Place::Room,
        };
        let event = self.chat.post(&p.room, &text, &author, place, &txn).await?;
        {
            let t = self.topic_mut(st, p, number);
            t.card.done = true;
            t.card_event = Some(event.clone());
            t.card_text = Some(text);
        }
        self.save(st).await?;
        self.add_buttons(st, p, number).await;
        Ok(())
    }

    /// 👍 and 🤝 under a topic's message (again at the next tick when one failed; the same
    /// transaction IDs, so none comes twice).
    async fn add_buttons(&self, st: &mut State, p: &Plenum, number: u32) {
        let t = &st.book.plenums[&p.key].topics[&number];
        let (Some(event), Some(txn)) = (t.card_event.clone(), t.card.txn.clone()) else {
            return;
        };
        for (i, key) in [topics::INTEREST, topics::DECIDE].iter().enumerate() {
            if let Err(e) = self
                .chat
                .react(&p.room, &event, key, &format!("{txn}-{}", i + 1))
                .await
            {
                warn!(plenum = %p.key, topic = number, "A button under the topic: {e:#}");
                return;
            }
        }
        self.topic_mut(st, p, number).buttons = true;
        self.save_or_log(st).await;
    }

    fn topic_mut<'a>(&self, st: &'a mut State, p: &Plenum, number: u32) -> &'a mut Topic {
        st.book
            .plenums
            .get_mut(&p.key)
            .and_then(|a| a.topics.get_mut(&number))
            .expect("the topic is in the book")
    }

    /// `!top` alone.
    pub(super) fn topics_list(&self, st: &mut State, p: &Plenum, now: DateTime<Utc>) -> String {
        let next = self.upcoming(st, p, now, 1).into_iter().next();
        let open: Vec<Topic> = st
            .book
            .plenums
            .get(&p.key)
            .map(|a| a.open().cloned().collect())
            .unwrap_or_default();
        let mut for_next = Vec::new();
        let mut later = Vec::new();
        for t in &open {
            let item = (t, self.topic_link(p, t));
            match (self.target_of(st, p, t, now), &next) {
                (Some((id, _)), Some((next_id, _))) if &id == next_id => for_next.push(item),
                (None, None) => for_next.push(item),
                _ => later.push(item),
            }
        }
        topics::list_text(&p.name, next.map(|(_, k)| k), &for_next, &later)
    }

    /// `!top #12 <action>`.
    pub(super) async fn topic_action(
        &self,
        st: &mut State,
        p: &Plenum,
        sender: &Sender,
        number: u32,
        action: TopicAction,
        now: DateTime<Utc>,
    ) -> String {
        let Some(t) = st
            .book
            .plenums
            .get(&p.key)
            .and_then(|a| a.topics.get(&number))
            .cloned()
        else {
            return format!(
                "There is no topic #{number} in {}. `!top` lists them.",
                p.name
            );
        };
        if !self.may_act(p, &t, &sender.id) {
            return self.who_may(p, &t);
        }
        let answer = match action {
            TopicAction::Withdraw if t.is_open() => {
                self.topic_mut(st, p, number).close(Status::Withdrawn, now);
                format!("🗑️ #{number} is withdrawn.")
            }
            TopicAction::Done if t.is_open() => {
                self.topic_mut(st, p, number).close(Status::Done, now);
                format!("✅ #{number} is marked as dealt with.")
            }
            TopicAction::Withdraw | TopicAction::Done => {
                format!("#{number} is not open any more (`!top #{number} again` brings it back).")
            }
            TopicAction::Again if t.is_open() => {
                format!("#{number} is on the agenda already.")
            }
            TopicAction::Again if t.deleted => {
                format!("#{number} was deleted; please bring it in anew with `!top`.")
            }
            TopicAction::Again => {
                let t = self.topic_mut(st, p, number);
                t.reopen();
                t.not_after = None;
                format!("↩️ #{number} goes onto the next agenda again.")
            }
            TopicAction::Later if !t.is_open() => {
                format!("#{number} is not open (`!top #{number} again` first).")
            }
            TopicAction::Later => {
                let Some((id, start)) = self.target_of(st, p, &t, now) else {
                    return format!(
                        "{} has no meeting in the calendar yet: #{number} waits anyway.",
                        p.name
                    );
                };
                let was_placed = t.placed.as_deref() == Some(id.as_str());
                let t = self.topic_mut(st, p, number);
                t.not_after = Some(start);
                t.placed = None;
                t.short_notice = false;
                let mut s = format!(
                    "⏭️ #{number} moves to the plenum after {}.",
                    topics::short_meeting(start)
                );
                if was_placed {
                    s.push_str(" Its section stays in those minutes; you can cross it out there.");
                }
                s
            }
            TopicAction::Decide | TopicAction::Talk if !t.is_open() => {
                format!("#{number} is not open any more.")
            }
            TopicAction::Decide => {
                self.topic_mut(st, p, number).decide = true;
                format!("🤝 #{number} is a topic to decide.")
            }
            TopicAction::Talk => {
                self.topic_mut(st, p, number).decide = false;
                if t.tap_decide {
                    format!("#{number} is to talk about now, but the 🤝 under its message still makes it one to decide: tap 🤝 again to remove it.")
                } else {
                    format!("💬 #{number} is a topic to talk about.")
                }
            }
        };
        info!(plenum = %p.key, topic = number, ?action, "Topic changed");
        answer
    }

    /// The plenum a settings command is for: in its room by the key or alone, in an admin's
    /// direct chat by its key.
    pub(super) fn pick_for_settings<'a>(
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
    pub(super) fn settings_text(
        &self,
        st: &State,
        p: &Plenum,
        user: &UserId,
        room: &RoomId,
    ) -> String {
        let empty = BTreeMap::new();
        let overrides = st
            .book
            .plenums
            .get(&p.key)
            .map(|a| &a.settings)
            .unwrap_or(&empty);
        plenum_settings::describe(
            p,
            overrides,
            self.may_organize(p, user),
            &self.key_hint(p, room),
        )
    }

    /// `!plenum set <name> <value>` (`value` None: reset). The answer says what changed.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn change_setting(
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
        let value = match value {
            Some(raw) => match plenum_settings::normalize(name, raw) {
                Ok(v) => Some(v),
                Err(why) => return why,
            },
            None => None,
        };
        if let Some(why) = value
            .as_deref()
            .and_then(|v| plenum_settings::refused(p, name, v))
        {
            return why;
        }
        if value.is_none() && !p.topics.room_settings {
            return plenum_settings::refused(p, name, "").unwrap_or_default();
        }
        let area = st.book.plenums.entry(p.key.clone()).or_default();
        let today = area
            .changes
            .iter()
            .filter(|c| c.by_user == sender.id.as_str() && now - c.at < Duration::days(1))
            .count();
        let all_today = area
            .changes
            .iter()
            .filter(|c| now - c.at < Duration::days(1))
            .count();
        if !self.admins.contains(&sender.id) {
            if today >= 10 {
                return "⏳ That is 10 changes from you today; more tomorrow, please.".to_owned();
            }
            if all_today >= 30 {
                return "⏳ This plenum's settings changed 30 times today; more tomorrow, please."
                    .to_owned();
            }
        }
        let before = plenum_settings::current(p, &area.settings, name);
        match &value {
            Some(v) => {
                area.settings.insert(name.to_owned(), v.clone());
            }
            None => {
                area.settings.remove(name);
            }
        }
        let after = plenum_settings::current(p, &area.settings, name);
        if before == after {
            let _ = self.save(st).await;
            return format!("`{name}` is **{after}** already.");
        }
        let by = self.shown_name(sender);
        area.changes.push(topics::Change {
            by: by.clone(),
            by_user: sender.id.to_string(),
            name: name.to_owned(),
            from: before.clone(),
            to: after.clone(),
            at: now,
        });
        if let Err(e) = self.save(st).await {
            error!("{e:#}");
            // undone: what is not saved must not seem changed
            let area = st.book.plenums.entry(p.key.clone()).or_default();
            area.changes.pop();
            match &value {
                Some(_) => {
                    area.settings.remove(name);
                }
                None => {
                    area.settings.insert(name.to_owned(), before);
                }
            }
            return "⚠️ The setting could not be saved just now. Please try again.".to_owned();
        }
        info!(plenum = %p.key, setting = name, from = %before, to = %after, "Setting changed in the room");
        let what = match value {
            Some(_) => "",
            None => " (the default again)",
        };
        let said = format!(
            "⚙️ {by} changed `{name}` of {}: {before} → **{after}**{what}. `!plenum settings` shows them all.",
            topics::md(&p.name)
        );
        // changed elsewhere (an admin's direct chat): the plenum's room hears it too
        if room != p.room {
            let txn = format!("plenum-setting-{}", uuid::Uuid::new_v4());
            if let Err(e) = self.chat.send(&p.room, &said, &txn).await {
                warn!(plenum = %p.key, "Telling the room about a setting: {e:#}");
            }
        }
        said
    }

    /// The topics to put into the minutes of meeting `key` when they are made.
    pub(super) fn due_for(&self, st: &State, p: &Plenum, key: &str) -> Vec<u32> {
        let ps = &st.plenums[&p.key];
        let Some(start) = ps.meetings.get(key).and_then(|m| m.start) else {
            return Vec::new();
        };
        // an earlier meeting whose minutes are open takes its topics first (place_topics)
        let earlier: Vec<MeetingKey> = ps
            .meetings
            .values()
            .filter(|m| m.created && !m.close.done && !m.cancelled && !m.skipped)
            .filter_map(|m| m.start)
            .filter(|s| *s < start)
            .collect();
        let eligible = |t: &Topic, at: MeetingKey| t.not_after.is_none_or(|after| at > after);
        st.book
            .plenums
            .get(&p.key)
            .map(|a| {
                a.open()
                    .filter(|t| t.placed.is_none())
                    .filter(|t| eligible(t, start))
                    .filter(|t| !earlier.iter().any(|s| eligible(t, *s)))
                    .map(|t| t.number)
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The link to a topic's message, if it has one.
    fn topic_link(&self, p: &Plenum, t: &Topic) -> Option<String> {
        t.card_event
            .as_deref()
            .map(|e| topics::matrix_link(p.room.as_str(), e))
    }

    /// A new protocol's text with the topics in it (agenda lines and sections).
    pub(super) fn with_topics(
        &self,
        st: &State,
        p: &Plenum,
        numbers: &[u32],
        text: String,
    ) -> String {
        let mut text = text;
        let Some(area) = st.book.plenums.get(&p.key) else {
            return text;
        };
        for n in numbers {
            let Some(t) = area.topics.get(n) else {
                continue;
            };
            text = protocol::insert_top(&text, &topics::agenda_line(t));
            let link = self.topic_link(p, t);
            text = topics::insert_section(&text, &topics::section(t, link.as_deref()));
        }
        text
    }

    /// The agenda of meeting `key` for its announcement (empty without topics).
    pub(super) fn agenda_of(&self, st: &State, p: &Plenum, key: &str) -> String {
        let Some(area) = st.book.plenums.get(&p.key) else {
            return String::new();
        };
        let placed: Vec<&Topic> = area
            .open()
            .filter(|t| t.placed.as_deref() == Some(key))
            .collect();
        let items: Vec<topics::AgendaItem<'_>> = placed
            .iter()
            .map(|t| topics::AgendaItem {
                topic: t,
                link: self.topic_link(p, t),
            })
            .collect();
        topics::agenda(&items)
    }

    /// The announcement's text with the agenda.
    pub(super) fn announcement_with_agenda(
        &self,
        st: &State,
        p: &Plenum,
        key: &str,
        head: &str,
        shared: Publication,
    ) -> String {
        // a topic goes in by `!top` (a line written into the document is no topic)
        let head = head.replace(
            ", reply to a message with `!top`, or write it into the document.",
            ", or reply to a message with `!top`.",
        );
        let agenda = self.agenda_of(st, p, key);
        if agenda.is_empty() || shared == Publication::Confidential {
            return head;
        }
        // the agenda right under the title, where a phone shows it
        match head.split_once("\n\n") {
            Some((title, rest)) => format!("{title}\n\n{agenda}\n\n{rest}"),
            None => format!("{head}\n\n{agenda}"),
        }
    }

    /// Takes what came in from Matrix since the last tick.
    pub(super) fn take_inbox(&self) -> Vec<Inbound> {
        std::mem::take(&mut *self.inbox.lock().unwrap())
    }

    /// What came in, for every plenum (each item only in its own room).
    pub(super) fn handle_inbound(&self, st: &mut State, items: &[Inbound], now: DateTime<Utc>) {
        for item in items {
            let room = match item {
                Inbound::Reaction { room, .. }
                | Inbound::Redaction { room, .. }
                | Inbound::Edit { room, .. }
                | Inbound::Thread { room, .. } => room,
            };
            for base in self.plenums.iter().filter(|p| &p.room == room) {
                let p = self.effective(base, st);
                self.handle_one(st, &p, item, now);
            }
        }
    }

    fn handle_one(&self, st: &mut State, p: &Plenum, item: &Inbound, now: DateTime<Utc>) {
        let keys: Vec<&str> = self
            .plenums_in(&p.room)
            .iter()
            .map(|p| p.key.as_str())
            .collect();
        let Some(area) = st.book.plenums.get(&p.key) else {
            return;
        };
        match item {
            Inbound::Reaction { target, .. } => {
                if let Some(n) = area.by_event(target) {
                    self.topic_mut(st, p, n).dirty = true;
                }
            }
            Inbound::Redaction {
                redacts, sender, ..
            } => {
                let source = area
                    .topics
                    .values()
                    .find(|t| {
                        t.source.as_deref() == Some(redacts)
                            || t.card_event.as_deref() == Some(redacts)
                            || t.from_event.as_deref() == Some(redacts)
                    })
                    .cloned();
                let proposal = area
                    .topics
                    .values()
                    .find(|t| t.proposal_event.as_deref() == Some(redacts))
                    .map(|t| t.number);
                match (source, proposal) {
                    (Some(t), _) if self.may_act(p, &t, sender) => {
                        let topic = self.topic_mut(st, p, t.number);
                        if topic.is_open() {
                            topic.close(Status::Withdrawn, now);
                        }
                        // the message it came from is gone: so are its words
                        if topic.source.as_deref() == Some(redacts)
                            || topic.from_event.as_deref() == Some(redacts)
                        {
                            topic.deleted = true;
                            topic.title = "(deleted)".to_owned();
                            topic.details.clear();
                            topic.proposal = None;
                        }
                        info!(plenum = %p.key, topic = t.number, "Topic withdrawn by a deletion");
                    }
                    (Some(_), _) => {}
                    (None, Some(n)) => {
                        let t = self.topic_mut(st, p, n);
                        if t.proposal_by.as_deref() == Some(sender.as_str())
                            || self.admins.contains(sender)
                        {
                            t.proposal = None;
                            t.proposal_event = None;
                        }
                    }
                    // maybe a reaction taken back (its target is not said): the hourly count
                    // and the one before the outcome see it
                    (None, None) => {}
                }
            }
            Inbound::Edit {
                original,
                sender,
                body,
                ..
            } => {
                let found = area.topics.values().find(|t| {
                    t.source.as_deref() == Some(original)
                        || t.proposal_event.as_deref() == Some(original)
                });
                let Some(t) = found.cloned() else {
                    return;
                };
                if !t.is_open() {
                    return;
                }
                if t.source.as_deref() == Some(original) {
                    // only its own sender, and not someone else's message brought in by a reply
                    if t.by_user.as_deref() != Some(sender.as_str()) || t.from.is_some() {
                        return;
                    }
                    let Some(s) = top_text(body, &keys).and_then(topics::parse_submission) else {
                        return;
                    };
                    let topic = self.topic_mut(st, p, t.number);
                    topic.title = s.title;
                    topic.details = s.details;
                    topic.decide = s.decide || topic.decide;
                    if topic.proposal_event.is_none() {
                        topic.proposal = s.proposal;
                    }
                } else if t.proposal_by.as_deref() == Some(sender.as_str()) {
                    if let Some(proposal) = topics::proposal_in(body) {
                        self.topic_mut(st, p, t.number).proposal = Some(proposal);
                    }
                }
            }
            Inbound::Thread {
                root,
                event,
                sender,
                body,
                ..
            } => {
                let Some(t) = area
                    .topics
                    .values()
                    .find(|t| t.card_event.as_deref() == Some(root))
                    .cloned()
                else {
                    return;
                };
                let Some(proposal) = topics::proposal_in(body) else {
                    return;
                };
                // the proposal is the author's
                if !t.is_open() || !self.is_author(&t, sender) {
                    return;
                }
                let topic = self.topic_mut(st, p, t.number);
                topic.proposal = Some(proposal);
                topic.proposal_event = Some(event.clone());
                topic.proposal_by = Some(sender.to_string());
                info!(plenum = %p.key, topic = t.number, "A proposal from the thread");
            }
        }
    }

    /// The topics' part of a tick, for plenum `p` (with its room's settings).
    pub(super) async fn tick_topics(&self, st: &mut State, p: &Plenum, now: DateTime<Utc>) {
        if !p.topics.enabled {
            return;
        }
        if let Err(e) = self.post_cards(st, p, now).await {
            warn!(plenum = %p.key, "Topic messages: {e:#}");
        }
        self.recount(st, p, now).await;
        self.unplace_cancelled(st, p);
        if let Err(e) = self.place_topics(st, p, now).await {
            warn!(plenum = %p.key, "Topics into the minutes: {e:#}");
        }
        if let Err(e) = self.outcomes(st, p, now).await {
            warn!(plenum = %p.key, "Topic outcomes: {e:#}");
        }
        self.refresh_cards(st, p, now).await;
        self.refresh_announcements(st, p).await;
    }

    /// Messages that could not be posted when the topic came.
    async fn post_cards(&self, st: &mut State, p: &Plenum, now: DateTime<Utc>) -> Result<()> {
        let due: Vec<u32> = st
            .book
            .plenums
            .get(&p.key)
            .map(|a| {
                a.open()
                    .filter(|t| !t.card.done || !t.buttons)
                    .map(|t| t.number)
                    .collect()
            })
            .unwrap_or_default();
        for n in due {
            if st.book.plenums[&p.key].topics[&n].card.done {
                self.add_buttons(st, p, n).await;
            } else {
                self.post_card(st, p, n, now).await?;
            }
        }
        Ok(())
    }

    /// A topic's 🤝 (from its author or an organiser), counted from the homeserver.
    async fn recount(&self, st: &mut State, p: &Plenum, now: DateTime<Utc>) {
        let due: Vec<(u32, String)> = st
            .book
            .plenums
            .get(&p.key)
            .map(|a| {
                a.open()
                    .filter(|t| t.dirty || t.counted_at.is_none_or(|at| now - at >= RECOUNT_EVERY))
                    .filter_map(|t| Some((t.number, t.card_event.clone()?)))
                    .collect()
            })
            .unwrap_or_default();
        for (n, event) in due {
            match self.chat.reactions(&p.room, &event).await {
                Ok(list) => {
                    let t = st.book.plenums[&p.key].topics[&n].clone();
                    let tap = list.iter().any(|(who, key)| {
                        mxbot_common::reaction::is_one_of(key, &[topics::DECIDE])
                            && self.me.as_deref() != Some(who.as_ref())
                            && self.may_act(p, &t, who)
                    });
                    // as many as the bot reads: a flood of reactions could hide the tap
                    let complete = list.len() < REACTIONS_READ;
                    let t = self.topic_mut(st, p, n);
                    if complete || tap {
                        t.tap_decide = tap;
                    }
                    t.dirty = false;
                    t.counted_at = Some(now);
                }
                Err(e) => {
                    warn!(plenum = %p.key, topic = n, "Counting the reactions: {e:#}");
                    self.topic_mut(st, p, n).counted_at = Some(now);
                }
            }
        }
    }

    /// Topics placed in a meeting that was cancelled (or skipped) go to the next one.
    fn unplace_cancelled(&self, st: &mut State, p: &Plenum) {
        let gone: BTreeSet<String> = st
            .plenums
            .get(&p.key)
            .map(|ps| {
                ps.meetings
                    .iter()
                    .filter(|(_, m)| (m.cancelled || m.skipped) && !m.close.done)
                    .map(|(id, _)| id.clone())
                    .collect()
            })
            .unwrap_or_default();
        if gone.is_empty() {
            return;
        }
        if let Some(area) = st.book.plenums.get_mut(&p.key) {
            for t in area
                .topics
                .values_mut()
                .filter(|t| t.status == Status::Open)
            {
                if t.placed.as_ref().is_some_and(|id| gone.contains(id)) {
                    info!(plenum = %p.key, topic = t.number, "Its meeting is cancelled: to the next one");
                    t.placed = None;
                    t.short_notice = false;
                }
            }
        }
    }

    /// Open topics into the minutes of their meeting once those exist.
    async fn place_topics(&self, st: &mut State, p: &Plenum, now: DateTime<Utc>) -> Result<()> {
        let open: Vec<Topic> = st
            .book
            .plenums
            .get(&p.key)
            .map(|a| a.open().filter(|t| t.placed.is_none()).cloned().collect())
            .unwrap_or_default();
        for t in open {
            let Some((id, _)) = self.target_of(st, p, &t, now) else {
                continue;
            };
            let Some(m) = st.plenums[&p.key].meetings.get(&id).cloned() else {
                continue;
            };
            if !m.created || m.close.done || m.close.requested {
                continue;
            }
            let Some(doc) = self.wiki.doc(&m.doc_id).await? else {
                continue;
            };
            {
                let topic = self.topic_mut(st, p, t.number);
                topic.short_notice = m.announce.done && topic.carried == 0;
            }
            let t = st.book.plenums[&p.key].topics[&t.number].clone();
            if !topics::has_agenda_line(&doc.text, t.number)
                && self
                    .add_lines(&m.doc_id, &[topics::agenda_line(&t)])
                    .await?
                    .is_none()
            {
                continue;
            }
            if !topics::has_section(&doc.text, t.number) {
                let link = self.topic_link(p, &t);
                let section = topics::section(&t, link.as_deref());
                let marker = topics::SECTIONS_MARKER;
                if !self
                    .wiki
                    .patch(&m.doc_id, marker, &format!("{section}\n{marker}"))
                    .await?
                {
                    self.wiki
                        .append(&m.doc_id, &format!("\n\n{section}"))
                        .await?;
                }
            }
            self.topic_mut(st, p, t.number).placed = Some(id.clone());
            info!(plenum = %p.key, topic = t.number, meeting = %id, "Topic into the minutes");
            self.save(st).await?;
        }
        Ok(())
    }

    /// Each topic's message shows what it is now (edited when that changed).
    async fn refresh_cards(&self, st: &mut State, p: &Plenum, now: DateTime<Utc>) {
        let all: Vec<Topic> = st
            .book
            .plenums
            .get(&p.key)
            .map(|a| {
                a.topics
                    .values()
                    .filter(|t| t.card.done && t.card_event.is_some())
                    .filter(|t| {
                        t.is_open() || t.closed_at.is_some_and(|at| now - at < Duration::days(2))
                    })
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        for t in all {
            let target = self.card_target(st, p, &t, now);
            let text = topics::card(&t, target, p.topics.tell_interested);
            if t.card_text.as_deref() == Some(text.as_str()) {
                continue;
            }
            let event = t.card_event.clone().expect("filtered");
            // a new transaction ID for every edit, saved before it is sent
            let n = {
                let t = self.topic_mut(st, p, t.number);
                t.card_edits += 1;
                t.card_edits
            };
            if let Err(e) = self.save(st).await {
                warn!(plenum = %p.key, "{e:#}");
                return;
            }
            let author: Vec<OwnedUserId> = t
                .by_user
                .as_deref()
                .and_then(|u| OwnedUserId::try_from(u).ok())
                .into_iter()
                .collect();
            match self
                .chat
                .edit(&p.room, &event, &text, &author, &edit_txn(&event, n))
                .await
            {
                Ok(()) => self.topic_mut(st, p, t.number).card_text = Some(text),
                Err(e) => {
                    // Matrix is down: the other messages wait for the next tick too
                    warn!(plenum = %p.key, topic = t.number, "Editing the topic's message: {e:#}");
                    return;
                }
            }
        }
    }

    /// The announcements of open meetings follow their agenda.
    async fn refresh_announcements(&self, st: &mut State, p: &Plenum) {
        let posted: Vec<(String, Posted)> = st
            .book
            .plenums
            .get(&p.key)
            .map(|a| a.announcements.clone().into_iter().collect())
            .unwrap_or_default();
        for (id, posted) in posted {
            let Some(m) = st.plenums[&p.key].meetings.get(&id).cloned() else {
                if let Some(a) = st.book.plenums.get_mut(&p.key) {
                    a.announcements.remove(&id);
                }
                continue;
            };
            if m.close.done || m.cancelled || m.skipped {
                if let Some(a) = st.book.plenums.get_mut(&p.key) {
                    a.announcements.remove(&id);
                }
                continue;
            }
            // to be announced again (it moved): the new announcement, not this one, follows
            let Some(start) = m.start.filter(|_| m.announce.done) else {
                continue;
            };
            let shared = publication(p, &m);
            let head =
                protocol::announcement(p, start, &self.link_of(&m), 0, posted.moved_from, shared);
            let text = self.announcement_with_agenda(st, p, &id, &head, shared);
            if text == posted.text {
                continue;
            }
            let n = posted.edits + 1;
            if let Some(a) = st
                .book
                .plenums
                .get_mut(&p.key)
                .and_then(|a| a.announcements.get_mut(&id))
            {
                a.edits = n;
            }
            if let Err(e) = self.save(st).await {
                warn!(plenum = %p.key, "{e:#}");
                return;
            }
            match self
                .chat
                .edit(
                    &p.room,
                    &posted.event,
                    &text,
                    &[],
                    &edit_txn(&posted.event, n),
                )
                .await
            {
                Ok(()) => {
                    if let Some(a) = st
                        .book
                        .plenums
                        .get_mut(&p.key)
                        .and_then(|a| a.announcements.get_mut(&id))
                    {
                        a.text = text;
                    }
                }
                Err(e) => {
                    warn!(plenum = %p.key, meeting = %id, "Editing the announcement: {e:#}");
                    return;
                }
            }
        }
    }

    /// Who hears a topic's outcome: its author and (with `tell_interested`) whoever gave it a
    /// 👍, if they are still in the room; at most [`topics::MAX_MENTIONS`].
    async fn listeners(&self, p: &Plenum, t: &Topic) -> Vec<OwnedUserId> {
        let members: HashSet<OwnedUserId> = match self.chat.members(&p.room).await {
            Ok(list) => list
                .into_iter()
                .filter(|m| m.joined)
                .map(|m| m.id)
                .collect(),
            Err(e) => {
                warn!(plenum = %p.key, "The room's members: {e:#}");
                return Vec::new();
            }
        };
        let mut out: Vec<OwnedUserId> = Vec::new();
        if let Some(by) = t
            .by_user
            .as_deref()
            .and_then(|u| OwnedUserId::try_from(u).ok())
        {
            if members.contains(&by) {
                out.push(by);
            }
        }
        if p.topics.tell_interested {
            if let Some(event) = &t.card_event {
                match self.chat.reactions(&p.room, event).await {
                    Ok(list) => {
                        for (who, key) in list {
                            if mxbot_common::reaction::is_one_of(&key, &[topics::INTEREST])
                                && self.me.as_ref() != Some(&who)
                                && members.contains(&who)
                                && !out.contains(&who)
                            {
                                out.push(who);
                            }
                        }
                    }
                    Err(e) => warn!(plenum = %p.key, topic = t.number, "Counting the 👍: {e:#}"),
                }
            }
        }
        out.truncate(topics::MAX_MENTIONS);
        out
    }

    /// After a meeting is closed: what came of each of its topics, into their threads.
    async fn outcomes(&self, st: &mut State, p: &Plenum, now: DateTime<Utc>) -> Result<()> {
        let closed: Vec<(String, Meeting)> = st.plenums[&p.key]
            .meetings
            .iter()
            .filter(|(_, m)| m.close.done && !m.cancelled)
            .map(|(id, m)| (id.clone(), m.clone()))
            .collect();
        for (id, m) in closed {
            let numbers: Vec<u32> = st
                .book
                .plenums
                .get(&p.key)
                .map(|a| {
                    a.open()
                        .filter(|t| t.placed.as_deref() == Some(id.as_str()))
                        .map(|t| t.number)
                        .collect()
                })
                .unwrap_or_default();
            if numbers.is_empty() {
                continue;
            }
            let Some(start) = m.start else {
                continue;
            };
            let text = self
                .wiki
                .doc(&m.doc_id)
                .await?
                .map(|d| d.text)
                .unwrap_or_default();
            let sections = topics::sections(&text);
            // Nobody wrote in a topic's section, but decisions are elsewhere: the minutes were
            // written the old way. Nobody knows which topic was reached; none is carried on.
            let used = numbers.iter().any(|n| {
                sections
                    .get(n)
                    .is_some_and(|s| s.written || !s.decisions.is_empty())
            });
            let elsewhere = !protocol::extract(&text).decisions.is_empty();
            let shared = publication(p, &m);
            let link = match shared {
                Publication::Confidential => String::new(),
                _ => self.link_of(&m),
            };
            for n in numbers {
                let t = st.book.plenums[&p.key].topics[&n].clone();
                let section = sections.get(&n);
                let result = match section {
                    Some(s) if !s.decisions.is_empty() => topics::Result::Decided,
                    Some(s) if s.written => topics::Result::Discussed,
                    Some(_) if !used && elsewhere => topics::Result::Unknown,
                    Some(_) => topics::Result::NotReached,
                    None => topics::Result::Unknown,
                };
                let carry = result == topics::Result::NotReached && p.topics.carry_over;
                let next = if carry {
                    self.upcoming(st, p, now, 12)
                        .into_iter()
                        .map(|(_, k)| k)
                        .find(|k| *k > start && t.not_after.is_none_or(|a| *k > a))
                } else {
                    None
                };
                let (decisions, tasks) = section
                    .map(|s| (s.decisions.clone(), s.tasks.clone()))
                    .unwrap_or_default();
                let message = topics::outcome_text(
                    &t,
                    result,
                    start.date(),
                    &decisions,
                    &tasks,
                    &link,
                    shared,
                    next,
                    carry,
                );
                if let (Some(message), Some(card)) = (message, t.card_event.clone()) {
                    if !t.outcome.done && t.outcome_attempts < topics::OUTCOME_ATTEMPTS {
                        let txn = self.topic_mut(st, p, n).outcome.txn();
                        self.topic_mut(st, p, n).outcome_attempts += 1;
                        self.save(st).await?;
                        let users = match result {
                            topics::Result::Decided | topics::Result::Discussed => {
                                self.listeners(p, &t).await
                            }
                            _ => Vec::new(),
                        };
                        match self
                            .chat
                            .post(&p.room, &message, &users, Place::Thread(&card), &txn)
                            .await
                        {
                            Ok(_) => self.topic_mut(st, p, n).outcome.done = true,
                            Err(e) => {
                                warn!(plenum = %p.key, topic = n, "The outcome into the thread: {e:#}");
                                if self.topic_mut(st, p, n).outcome_attempts
                                    < topics::OUTCOME_ATTEMPTS
                                {
                                    continue; // again at the next tick
                                }
                            }
                        }
                    }
                }
                let topic = self.topic_mut(st, p, n);
                let told = topic.outcome.done;
                topic.history.push(Outcome {
                    meeting: id.clone(),
                    date: start.date(),
                    result,
                    link: link.clone(),
                    confidential: shared == Publication::Confidential,
                    told,
                });
                match result {
                    topics::Result::Decided | topics::Result::Discussed => {
                        topic.close(Status::Done, now)
                    }
                    topics::Result::NotReached if carry => {
                        topic.carried += 1;
                        topic.reopen();
                    }
                    _ => topic.close(Status::Unclear, now),
                }
                info!(plenum = %p.key, topic = n, meeting = %id, ?result, "Topic outcome");
                self.save(st).await?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
