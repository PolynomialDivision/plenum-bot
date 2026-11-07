//! Topics (docs/TOPICS.md): what `!top` brings in, kept per plenum in `store/topics.json`.
//!
//! A topic is numbered (`#12`) and has a message of its own in the plenum's room, the bot's,
//! which is its thread's root, the target of 👍 and its record (the bot edits it as the topic
//! changes and when it is dealt with). It goes onto the agenda of the next meeting (an agenda
//! line and a section of its own in the minutes, both written by the bot), the meeting's
//! announcement lists it, and after the meeting its section says what came of it; that goes
//! into its thread.
//!
//! This module is the model and the texts, pure functions; the engine's part is
//! `topic_flow.rs`. Everything from people that goes into the minutes is flattened and quoted
//! ([`wiki_line`]): no heading, checkbox, "Decision:" or "Task:" can come from it.

use std::collections::BTreeMap;

use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};

use crate::{config::Publication, protocol, schedule::MeetingKey, state::Step};

/// Where the bot puts the topics' sections in the minutes (the template's place for them; a
/// protocol the bot makes always gets it).
pub const SECTIONS_MARKER: &str = "➕ Sections for the topics go here (plenum-bot).";

/// The reaction that says "this matters to me".
pub const INTEREST: &str = "👍";
/// The author's (or an organiser's) reaction that makes a topic one to decide.
pub const DECIDE: &str = "🤝";

pub const MAX_TITLE: usize = 200;
pub const MAX_DETAILS: usize = 1500;
pub const MAX_PROPOSAL: usize = 1000;
pub const MAX_NAME: usize = 60;
/// Topics one person may bring in a day, and open topics a plenum may have.
pub const PER_PERSON_A_DAY: usize = 10;
pub const MAX_OPEN: usize = 60;
/// Open topics one person may have in a plenum (so nobody fills the plenum's).
pub const MAX_OPEN_PER_PERSON: usize = 15;
/// People mentioned with an outcome (the author first).
pub const MAX_MENTIONS: usize = 20;
/// Attempts at a topic's outcome message before it is given up (the topic changes anyway).
pub const OUTCOME_ATTEMPTS: u32 = 3;
/// A done or withdrawn topic's texts are cleared after this many days, the topic itself is
/// dropped after the second.
pub const CLEAR_TEXTS_DAYS: i64 = 30;
pub const KEEP_DAYS: i64 = 180;

/// Every plenum's topics (store/topics.json, a file of its own: an older image neither reads
/// nor overwrites it).
#[derive(Serialize, Deserialize, Default, Debug, Clone, PartialEq)]
pub struct Book {
    #[serde(default)]
    pub plenums: BTreeMap<String, Area>,
}

#[derive(Serialize, Deserialize, Default, Debug, Clone, PartialEq)]
pub struct Area {
    /// The last number given (numbers are never reused).
    #[serde(default)]
    pub last: u32,
    #[serde(default)]
    pub topics: BTreeMap<u32, Topic>,
    /// The plenum's own settings, set in its room (plenum_settings.rs), raw.
    #[serde(default)]
    pub settings: BTreeMap<String, String>,
    /// Who changed which setting when (the last [`KEEP_CHANGES`]).
    #[serde(default)]
    pub changes: Vec<Change>,
    /// The announcement of each meeting (by its ID) as the bot posted it: edited when the
    /// agenda changes.
    #[serde(default)]
    pub announcements: BTreeMap<String, Posted>,
    /// The old `pending_tops` were moved here.
    #[serde(default)]
    pub migrated: bool,
}

/// Setting changes kept (for the overview and the daily limits): this many, at most
/// [`CLEAR_TEXTS_DAYS`] old.
pub const KEEP_CHANGES: usize = 200;

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct Posted {
    pub event: String,
    pub text: String,
    /// Edits sent so far (each edit's transaction ID is new, saved before it is sent).
    #[serde(default)]
    pub edits: u32,
    /// The date it said it moved from (kept for its edits).
    #[serde(default)]
    pub moved_from: Option<MeetingKey>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct Change {
    pub by: String,
    pub by_user: String,
    pub name: String,
    pub from: String,
    pub to: String,
    pub at: DateTime<Utc>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    #[default]
    Open,
    /// Dealt with (decided, talked about, or `!top #n done`).
    Done,
    Withdrawn,
    /// It was on an agenda and its section is gone from the minutes: nobody knows.
    Unclear,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Result {
    Decided,
    Discussed,
    NotReached,
    Unknown,
}

/// What came of a topic at one meeting.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct Outcome {
    pub meeting: String,
    pub date: NaiveDate,
    pub result: Result,
    #[serde(default)]
    pub link: String,
    /// The meeting was confidential: nothing of what came of it is shown.
    #[serde(default)]
    pub confidential: bool,
    /// Its thread heard it.
    #[serde(default)]
    pub told: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Default)]
pub struct Topic {
    pub number: u32,
    pub title: String,
    #[serde(default)]
    pub details: String,
    #[serde(default)]
    pub proposal: Option<String>,
    /// The thread message the proposal came from (its edits count, from its sender).
    #[serde(default)]
    pub proposal_event: Option<String>,
    #[serde(default)]
    pub proposal_by: Option<String>,
    /// To decide: by `decide:`, or `!top #n decide`.
    #[serde(default)]
    pub decide: bool,
    /// The author's (or an organiser's) 🤝 under the topic's message, as last counted.
    #[serde(default)]
    pub tap_decide: bool,
    /// The name shown for the author (the directory's, else the display name), cleaned.
    pub by: String,
    /// The account that brought it in (None: a topic from before, a `pending_tops` item).
    #[serde(default)]
    pub by_user: Option<String>,
    /// "Anna's message": brought in as a reply to someone else's message.
    #[serde(default)]
    pub from: Option<String>,
    /// That message's sender (who may withdraw the topic too) and the message (deleted: the
    /// topic is withdrawn).
    #[serde(default)]
    pub from_user: Option<String>,
    #[serde(default)]
    pub from_event: Option<String>,
    pub at: DateTime<Utc>,
    /// The `!top` message (its edits and its deletion).
    #[serde(default)]
    pub source: Option<String>,
    /// The topic's message: the thread root and the record.
    #[serde(default)]
    pub card: Step,
    #[serde(default)]
    pub card_event: Option<String>,
    /// What the topic's message shows now (edited when this changes).
    #[serde(default)]
    pub card_text: Option<String>,
    /// Edits of it sent so far (each edit's transaction ID is new, saved before it is sent).
    #[serde(default)]
    pub card_edits: u32,
    /// Its 👍 and 🤝 buttons are there.
    #[serde(default)]
    pub buttons: bool,
    /// Not before the meeting after this one (`!top #n later`).
    #[serde(default)]
    pub not_after: Option<MeetingKey>,
    /// The meeting whose minutes have it (its ID).
    #[serde(default)]
    pub placed: Option<String>,
    /// Placed after that meeting's agenda went out.
    #[serde(default)]
    pub short_notice: bool,
    #[serde(default)]
    pub status: Status,
    /// Not reached this many times (carried over).
    #[serde(default)]
    pub carried: u32,
    #[serde(default)]
    pub history: Vec<Outcome>,
    /// The outcome message for the meeting it was placed in.
    #[serde(default)]
    pub outcome: Step,
    #[serde(default)]
    pub outcome_attempts: u32,
    /// Its reactions are to be counted again (a reaction or redaction came in).
    #[serde(default)]
    pub dirty: bool,
    /// When its reactions were counted last.
    #[serde(default)]
    pub counted_at: Option<DateTime<Utc>>,
    /// The `!top` message was deleted: its texts are gone, it does not come back.
    #[serde(default)]
    pub deleted: bool,
    #[serde(default)]
    pub closed_at: Option<DateTime<Utc>>,
}

impl Topic {
    pub fn to_decide(&self) -> bool {
        self.decide || self.tap_decide
    }

    pub fn is_open(&self) -> bool {
        self.status == Status::Open
    }

    /// Done or withdrawn now: its texts go after [`CLEAR_TEXTS_DAYS`].
    pub fn close(&mut self, status: Status, now: DateTime<Utc>) {
        self.status = status;
        self.closed_at = Some(now);
    }

    /// Open again, for the next meeting.
    pub fn reopen(&mut self) {
        self.status = Status::Open;
        self.closed_at = None;
        self.placed = None;
        self.short_notice = false;
        self.outcome = Step::default();
        self.outcome_attempts = 0;
    }
}

impl Area {
    pub fn open(&self) -> impl Iterator<Item = &Topic> {
        self.topics.values().filter(|t| t.is_open())
    }

    /// The topic whose message (card) or `!top` message is `event`.
    pub fn by_event(&self, event: &str) -> Option<u32> {
        self.topics
            .values()
            .find(|t| t.card_event.as_deref() == Some(event) || t.source.as_deref() == Some(event))
            .map(|t| t.number)
    }

    pub fn next_number(&mut self) -> u32 {
        self.last += 1;
        self.last
    }

    /// Texts cleared, old topics dropped.
    pub fn prune(&mut self, now: DateTime<Utc>) {
        for t in self.topics.values_mut() {
            if let Some(at) = t.closed_at {
                if now - at > chrono::Duration::days(CLEAR_TEXTS_DAYS) {
                    t.details.clear();
                    t.proposal = None;
                }
            }
        }
        self.topics.retain(|_, t| {
            t.closed_at
                .is_none_or(|at| now - at <= chrono::Duration::days(KEEP_DAYS))
        });
        self.changes
            .retain(|c| now - c.at <= chrono::Duration::days(CLEAR_TEXTS_DAYS));
        let keep = self.changes.len().saturating_sub(KEEP_CHANGES);
        self.changes.drain(..keep);
    }
}

impl Book {
    pub async fn load(path: &std::path::Path) -> anyhow::Result<Self> {
        mxbot_common::persist::load_json_or_default(path).await
    }

    pub async fn save(&self, path: &std::path::Path) -> anyhow::Result<()> {
        mxbot_common::persist::save_json_atomic(path, self).await
    }
}

/// What a `!top` message says.
#[derive(Debug, PartialEq, Default)]
pub struct Submission {
    pub title: String,
    pub details: String,
    pub proposal: Option<String>,
    pub decide: bool,
}

/// `decide:` and its kin at the start of a topic: a topic to decide.
const DECIDE_PREFIXES: [&str; 5] = [
    "decide",
    "decision",
    "to decide",
    "entscheiden",
    "entscheidung",
];
const PROPOSAL_LABELS: [&str; 2] = ["proposal", "vorschlag"];

/// The text after `<label>:` at the start of `line` (any case, maybe in `**` or `_`).
fn after_any_label(line: &str, labels: &[&str]) -> Option<String> {
    let plain = line.trim().trim_start_matches(['*', '_']);
    for label in labels {
        let Some(head) = plain.get(..label.len()) else {
            continue;
        };
        if !head.eq_ignore_ascii_case(label) {
            continue;
        }
        let after = plain[label.len()..].trim_start_matches(['*', '_']);
        if let Some(after) = after.strip_prefix(':') {
            return Some(after.trim_start_matches(['*', '_']).trim().to_owned());
        }
    }
    None
}

/// A `!top` text (without `!top` and the plenum key): the first line is the title, maybe
/// starting with `decide:`; the further lines the description, from a line starting with
/// `Proposal:` (or `Vorschlag:`) on the proposal. None: no title.
pub fn parse_submission(text: &str) -> Option<Submission> {
    let mut lines = text.lines().map(str::trim).skip_while(|l| l.is_empty());
    let first = lines.next()?;
    let mut s = Submission::default();
    let title = match after_any_label(first, &DECIDE_PREFIXES) {
        Some(rest) => {
            s.decide = true;
            rest
        }
        None => first.to_owned(),
    };
    s.title = one_line(&title, MAX_TITLE);
    if s.title.is_empty() {
        return None;
    }
    let mut details = Vec::new();
    let mut proposal: Option<Vec<String>> = None;
    for line in lines {
        if let Some(rest) = after_any_label(line, &PROPOSAL_LABELS) {
            proposal = Some(vec![rest]);
            continue;
        }
        match proposal.as_mut() {
            Some(p) => p.push(line.to_owned()),
            None => details.push(line.to_owned()),
        }
    }
    s.details = cap(details.join("\n").trim(), MAX_DETAILS);
    s.proposal = proposal
        .map(|p| cap(p.join("\n").trim(), MAX_PROPOSAL))
        .filter(|p| !p.is_empty());
    Some(s)
}

/// A proposal in a thread message: its first line starts with `Proposal:` (or `Vorschlag:`).
pub fn proposal_in(body: &str) -> Option<String> {
    let mut lines = body.lines().map(str::trim).skip_while(|l| l.is_empty());
    let first = after_any_label(lines.next()?, &PROPOSAL_LABELS)?;
    let mut all = vec![first];
    all.extend(lines.map(str::to_owned));
    let text = cap(all.join("\n").trim(), MAX_PROPOSAL);
    (!text.is_empty()).then_some(text)
}

/// One line, at most `max` characters (an ellipsis when cut).
pub fn one_line(text: &str, max: usize) -> String {
    let joined = text.split_whitespace().collect::<Vec<_>>().join(" ");
    cap(&joined, max)
}

fn cap(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let mut s: String = text.chars().take(max.saturating_sub(1)).collect();
    s.push('…');
    s
}

/// Invisible characters a name could hide behind (bidi overrides, zero-width ones).
fn invisible(c: char) -> bool {
    c.is_control()
        || matches!(c,
            '\u{00AD}' | '\u{061C}' | '\u{180E}'
            | '\u{200B}'..='\u{200F}'
            | '\u{202A}'..='\u{202E}'
            | '\u{2060}'..='\u{2064}'
            | '\u{2066}'..='\u{206F}'
            | '\u{FEFF}')
}

/// A person's name as the bot writes it: visible characters, one line, at most
/// [`MAX_NAME`]; "someone" when nothing is left.
pub fn person_name(raw: &str) -> String {
    let visible: String = raw.chars().filter(|c| !invisible(*c)).collect();
    let name = one_line(&visible, MAX_NAME);
    if name.is_empty() {
        "someone".to_owned()
    } else {
        name
    }
}

/// Text from people in a chat message: Markdown has no effect (no links, images, headings or
/// emphasis), one line.
pub fn md(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in one_line(text, usize::MAX).chars() {
        if matches!(
            c,
            '\\' | '`' | '*' | '_' | '[' | ']' | '(' | ')' | '<' | '>' | '#' | '~' | '|' | '!'
        ) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// A line from people in the minutes: flattened like a `!top` item (protocol::top_line): no
/// brackets (Outline's checkboxes), no list, heading or quote syntax at its start, and one that
/// starts like a decision or a task is marked as text.
pub fn wiki_line(text: &str, max: usize) -> String {
    let mut s: String = one_line(text, max)
        .replace('[', "(")
        .replace(']', ")")
        // the bot's markers start so: a text from people never becomes one
        .replace('➕', "+")
        .trim_start_matches(['#', '>', '*', '-', '+', '|', '_', '='])
        .trim()
        .to_owned();
    // a numbered list ("1. ") or "↳" (the bot's own line) at the start, too
    let digits = s.chars().take_while(|c| c.is_ascii_digit()).count();
    if digits > 0 && s[digits..].starts_with(". ") {
        s = format!("({}){}", &s[..digits], &s[digits + 1..]);
    }
    if let Some(rest) = s.strip_prefix('↳') {
        s = rest.trim().to_owned();
    }
    if protocol::starts_like_decision_or_task(&s) {
        s = format!("Item: {s}");
    }
    s
}

/// A link to the topic's message in Matrix.
pub fn matrix_link(room: &str, event: &str) -> String {
    let via = room.split_once(':').map(|(_, s)| s).unwrap_or("");
    format!("https://matrix.to/#/{room}/{event}?via={via}")
}

/// Where a topic is going.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Target {
    Meeting(MeetingKey),
    /// No meeting in the calendar yet.
    Next,
}

fn kind_words(t: &Topic) -> &'static str {
    if t.to_decide() {
        "to decide"
    } else {
        "to talk about"
    }
}

fn by_line(t: &Topic) -> String {
    match &t.from {
        Some(from) => format!("{} ({}’s message)", md(&t.by), md(from)),
        None => md(&t.by),
    }
}

/// "Sun 1 Nov, 19:00"
pub fn short_meeting(key: MeetingKey) -> String {
    key.0.format("%a %-d %b, %H:%M").to_string()
}

/// The topic's message (the card), for its state now.
/// `tell`: whoever gives it a 👍 hears its outcome (`tell_interested`).
pub fn card(t: &Topic, target: Target, tell: bool) -> String {
    let head = format!("📌 **#{} {}**", t.number, md(&t.title));
    let last = t.history.last();
    match t.status {
        Status::Withdrawn => {
            return format!("{head}\nfrom {}\n🗑️ Withdrawn.", by_line(t));
        }
        Status::Done => {
            let said =
                match last.filter(|o| matches!(o.result, Result::Decided | Result::Discussed)) {
                    // a confidential meeting: not even whether it was decided
                    Some(o) if o.confidential => {
                        format!("✅ Dealt with at the plenum on {}.", day(o.date))
                    }
                    Some(o) => {
                        let what = if o.result == Result::Decided {
                            "✅ Decided"
                        } else {
                            "💬 Talked about"
                        };
                        let more = if o.told { " The thread has more." } else { "" };
                        format!("{what} at the plenum on {}.{more}", day(o.date))
                    }
                    None => "✅ Marked as dealt with.".to_owned(),
                };
            return format!("{head}\nfrom {}\n{said}", by_line(t));
        }
        Status::Unclear => {
            let when = last.map(|o| day(o.date)).unwrap_or_default();
            return format!(
                "{head}\nfrom {}\nIt was on the agenda of {when}. `!top #{} again` puts it on \
                 the next one.",
                by_line(t),
                t.number
            );
        }
        Status::Open => {}
    }
    let when = match target {
        Target::Meeting(key) => format!("for the plenum on {}", short_meeting(key)),
        Target::Next => "for the next plenum (not in the calendar yet)".to_owned(),
    };
    let mut text = format!("{head}\nfrom {} · {} · {when}", by_line(t), kind_words(t));
    if t.short_notice && t.to_decide() {
        text.push_str("\nShort notice: it came after the agenda went out.");
    }
    if t.carried > 0 {
        if let Some(o) = last {
            text.push_str(&format!(
                "\nNot reached on {}: on the agenda again.",
                day(o.date)
            ));
        }
    }
    if let Some(p) = &t.proposal {
        text.push_str(&format!("\nProposal: {}", md(&cap(p, 300))));
    }
    text.push_str(if tell {
        "\n\n👍 if it matters to you: you hear what came of it."
    } else {
        "\n\n👍 if it matters to you."
    });
    text.push_str(" Discuss it: reply in this message's thread.");
    if !t.to_decide() {
        text.push_str(&format!(
            " {}: tap 🤝 to make it a topic to decide.",
            md(&t.by)
        ));
    }
    if t.to_decide() && t.proposal.is_none() {
        text.push_str(&format!(
            " {}: reply in the thread with “Proposal: …” to add a proposal.",
            md(&t.by)
        ));
    }
    text
}

/// "16 Nov"
pub fn day(date: NaiveDate) -> String {
    date.format("%-d %b").to_string()
}

/// One item of an agenda in the chat.
pub struct AgendaItem<'a> {
    pub topic: &'a Topic,
    pub link: Option<String>,
}

/// The agenda in the announcement: decisions first, then the talks, each in the order they
/// came. Empty without topics.
pub fn agenda(items: &[AgendaItem<'_>]) -> String {
    let line = |i: &AgendaItem<'_>| {
        let t = i.topic;
        let number = match &i.link {
            Some(link) => format!("[#{}]({link})", t.number),
            None => format!("#{}", t.number),
        };
        let mut s = format!("• {number} {} ({})", md(&t.title), md(&t.by));
        if let Some(p) = &t.proposal {
            s.push_str(&format!(": proposal: {}", md(&cap(p, 150))));
        }
        if t.short_notice && t.to_decide() {
            s.push_str(" · short notice (came after the agenda)");
        }
        if t.carried > 0 {
            if let Some(o) = t.history.last() {
                s.push_str(&format!(" · again, not reached on {}", day(o.date)));
            }
        }
        s
    };
    let decide: Vec<String> = items
        .iter()
        .filter(|i| i.topic.to_decide())
        .map(line)
        .collect();
    let talk: Vec<String> = items
        .iter()
        .filter(|i| !i.topic.to_decide())
        .map(line)
        .collect();
    let mut out = Vec::new();
    if !decide.is_empty() {
        out.push(format!("**To decide:**\n{}", decide.join("\n")));
    }
    if !talk.is_empty() {
        out.push(format!("**To talk about:**\n{}", talk.join("\n")));
    }
    out.join("\n\n")
}

/// The topic's line on the agenda of the minutes (found again by its start, `* #12 `).
pub fn agenda_line(t: &Topic) -> String {
    let mut s = format!(
        "* #{} {} ({}) · {}",
        t.number,
        wiki_line(&t.title, MAX_TITLE),
        wiki_line(&t.by, MAX_NAME),
        kind_words(t)
    );
    if t.short_notice && t.to_decide() {
        s.push_str(" · short notice");
    }
    s
}

/// The topic's section in the minutes: its heading, what its author wrote (quoted), the link
/// to the discussion. People write below it.
pub fn section(t: &Topic, link: Option<&str>) -> String {
    let mut s = format!("### #{} {}\n\n", t.number, wiki_line(&t.title, MAX_TITLE));
    let mut who = format!("{}, {}", wiki_line(&t.by, MAX_NAME), kind_words(t));
    if let Some(from) = &t.from {
        who.push_str(&format!(" (from {}’s message)", wiki_line(from, MAX_NAME)));
    }
    if t.short_notice && t.to_decide() {
        who.push_str(", short notice");
    }
    if t.carried > 0 {
        if let Some(o) = t.history.last() {
            who.push_str(&format!(", not reached on {}", day(o.date)));
        }
    }
    s.push_str(&format!("> {who}\n"));
    for line in t.details.lines().filter(|l| !l.trim().is_empty()) {
        s.push_str(&format!("> {}\n", wiki_line(line, MAX_DETAILS)));
    }
    if let Some(p) = &t.proposal {
        let mut lines = p.lines().filter(|l| !l.trim().is_empty());
        if let Some(first) = lines.next() {
            s.push_str(&format!("> Proposal: {}\n", wiki_line(first, MAX_PROPOSAL)));
        }
        for line in lines {
            s.push_str(&format!("> {}\n", wiki_line(line, MAX_PROPOSAL)));
        }
    }
    if let Some(link) = link {
        s.push_str(&format!(
            "\n↳ Discussion in the chat: [#{}]({link})\n",
            t.number
        ));
    }
    s
}

/// `text` with `section` added where the topics' sections go: above [`SECTIONS_MARKER`]; without
/// it at the end of the "Minutes" section (with the marker after it); without that at the end
/// under "## Topics".
pub fn insert_section(text: &str, section: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let block: Vec<String> = section.trim_end().lines().map(str::to_owned).collect();
    let mut out: Vec<String> = lines.iter().map(|l| (*l).to_owned()).collect();
    let at = |i: usize, with_marker: bool, out: &mut Vec<String>| {
        let mut add = block.clone();
        add.push(String::new());
        if with_marker {
            add.push(SECTIONS_MARKER.to_owned());
            add.push(String::new());
        }
        out.splice(i..i, add);
    };
    if let Some(marker) = lines.iter().position(|l| l.trim() == SECTIONS_MARKER) {
        at(marker, false, &mut out);
        return join(out, text);
    }
    let minutes = lines.iter().position(|l| {
        let t = l.trim_start();
        t.starts_with("## ") && t[3..].trim().to_lowercase().starts_with("minutes")
    });
    if let Some(h) = minutes {
        let next = lines[h + 1..]
            .iter()
            .position(|l| {
                let t = l.trim_start();
                t.starts_with("# ") || t.starts_with("## ")
            })
            .map(|i| h + 1 + i);
        match next {
            Some(i) => {
                at(i, true, &mut out);
                return join(out, text);
            }
            None => {
                let mut t = text.trim_end().to_owned();
                t.push_str(&format!(
                    "\n\n{}\n\n{SECTIONS_MARKER}\n",
                    section.trim_end()
                ));
                return t;
            }
        }
    }
    let mut t = text.trim_end().to_owned();
    t.push_str(&format!(
        "\n\n## Topics\n\n{}\n\n{SECTIONS_MARKER}\n",
        section.trim_end()
    ));
    t
}

fn join(lines: Vec<String>, original: &str) -> String {
    let mut text = lines.join("\n");
    if original.ends_with('\n') && !text.ends_with('\n') {
        text.push('\n');
    }
    text
}

/// Outline may escape what it gives back (`\#12`, `\*`): compared without escapes.
fn unescaped(line: &str) -> String {
    line.replace('\\', "")
}

/// The topic number a heading names: `### #12 …` (only the number right after the hashes).
fn heading_topic(line: &str) -> Option<(usize, u32)> {
    // an escaped `\###` is text, not a heading; after the hashes Outline may escape `\#12`
    let t = line.trim_start();
    let level = t.chars().take_while(|c| *c == '#').count();
    if level == 0 || !t[level..].starts_with(' ') {
        return None;
    }
    let rest = unescaped(&t[level..]);
    let rest = rest.trim_start().strip_prefix('#')?;
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        return None;
    }
    let after = &rest[digits.len()..];
    if !(after.is_empty() || after.starts_with(' ')) {
        return None;
    }
    digits.parse().ok().map(|n| (level, n))
}

fn heading_level(line: &str) -> Option<usize> {
    let t = line.trim_start();
    let level = t.chars().take_while(|c| *c == '#').count();
    (level > 0 && t[level..].starts_with(' ')).then_some(level)
}

/// A topic's section in the closed minutes.
#[derive(Debug, Default, PartialEq)]
pub struct Section {
    /// Someone wrote in it (anything but the bot's quoted lines and its link).
    pub written: bool,
    pub decisions: Vec<String>,
    pub tasks: Vec<String>,
}

/// The topics' sections in the minutes, by number (the first heading with a number counts).
pub fn sections(markdown: &str) -> BTreeMap<u32, Section> {
    let lines: Vec<&str> = markdown.lines().collect();
    let mut out = BTreeMap::new();
    let mut i = 0;
    while i < lines.len() {
        let Some((level, n)) = heading_topic(lines[i]) else {
            i += 1;
            continue;
        };
        let end = lines[i + 1..]
            .iter()
            .position(|l| heading_level(l).is_some_and(|l| l <= level))
            .map(|j| i + 1 + j)
            .unwrap_or(lines.len());
        let body = &lines[i + 1..end];
        let written = body.iter().any(|l| {
            let t = unescaped(l.trim());
            !(t.is_empty() || t.starts_with('>') || t.starts_with('↳') || t == SECTIONS_MARKER)
        });
        let text: String = body
            .iter()
            .filter(|l| !l.trim_start().starts_with('>'))
            .copied()
            .collect::<Vec<_>>()
            .join("\n");
        let found = protocol::extract(&text);
        out.entry(n).or_insert(Section {
            written,
            decisions: found.decisions,
            tasks: found.tasks,
        });
        i = end;
    }
    out
}

/// Whether the minutes' agenda has the topic's line already.
pub fn has_agenda_line(markdown: &str, number: u32) -> bool {
    let start = [format!("* #{number} "), format!("- #{number} ")];
    markdown.lines().any(|l| {
        let t = unescaped(l.trim_start());
        start.iter().any(|s| t.starts_with(s.as_str()))
    })
}

/// Whether the minutes have the topic's section already.
pub fn has_section(markdown: &str, number: u32) -> bool {
    markdown
        .lines()
        .any(|l| heading_topic(l).is_some_and(|(_, n)| n == number))
}

/// The outcome message into a topic's thread. `decisions`, `tasks`: its section's (shown only
/// for `full`).
#[allow(clippy::too_many_arguments)]
pub fn outcome_text(
    t: &Topic,
    result: Result,
    date: NaiveDate,
    decisions: &[String],
    tasks: &[String],
    link: &str,
    publication: Publication,
    next: Option<MeetingKey>,
    carry: bool,
) -> Option<String> {
    let on = day(date);
    let n = t.number;
    let not_carried =
        format!("⏭️ #{n} not reached on {on}. `!top #{n} again` puts it on the next agenda.");
    if publication == Publication::Confidential {
        return match result {
            Result::Decided | Result::Discussed => {
                Some(format!("📌 #{n}: dealt with at the plenum on {on}."))
            }
            Result::NotReached if carry => Some(format!(
                "⏭️ #{n}: not reached on {on}; it is on the next agenda."
            )),
            Result::NotReached => Some(not_carried),
            Result::Unknown => None,
        };
    }
    let minutes = if link.is_empty() {
        String::new()
    } else {
        format!("\nMinutes: {link}")
    };
    match result {
        Result::Decided => {
            let mut s = format!("✅ #{} decided at the plenum on {on}.", t.number);
            if publication == Publication::Full {
                for d in decisions {
                    s.push_str(&format!("\n• {}", md(&cap(d, 500))));
                }
                if !tasks.is_empty() {
                    s.push_str("\nTasks:");
                    for task in tasks {
                        s.push_str(&format!("\n• {}", md(&cap(task, 300))));
                    }
                }
            }
            s.push_str(&minutes);
            Some(s.trim_end().to_owned())
        }
        Result::Discussed => Some(
            format!(
                "💬 #{} talked about at the plenum on {on}.{minutes}",
                t.number
            )
            .trim_end()
            .to_owned(),
        ),
        Result::NotReached if !carry => Some(not_carried),
        Result::NotReached => Some(match next {
            Some(key) => format!(
                "⏭️ #{} not reached on {on}: it is on the agenda of {}.",
                t.number,
                short_meeting(key)
            ),
            None => format!(
                "⏭️ #{} not reached on {on}: it is on the next agenda.",
                t.number
            ),
        }),
        Result::Unknown => None,
    }
}

/// The overview's part about topics (the wiki page the bot rewrites): how they work, the
/// ones waiting, the settings the room made (with who changed them last).
pub fn overview_block(open: &[&Topic], changed: &[(String, String)], changes: &[Change]) -> String {
    let mut t = String::from(
        "\n## Bringing in a topic\n\nWrite `!top <topic>` in the plenum's chat (or reply to a \
         message with `!top`). The topic gets a number and a message of its own: give it a 👍 if \
         it matters to you (you hear what came of it), and talk about it in that message's \
         thread. `!top decide: …` makes it a topic to decide; a line starting with `Proposal:` \
         is its proposal. The agenda comes with the announcement, each topic has its own \
         section in the minutes, and after the meeting its thread hears what came of it. \
         `!top` alone lists the topics.\n\n## Topics waiting\n\n",
    );
    if open.is_empty() {
        t.push_str("None.\n");
    }
    for topic in open {
        t.push_str(&format!(
            "* #{} {} ({}) · {}\n",
            topic.number,
            wiki_line(&topic.title, MAX_TITLE),
            wiki_line(&topic.by, MAX_NAME),
            kind_words(topic)
        ));
    }
    if !changed.is_empty() {
        t.push_str("\n## Settings made in the chat\n\n");
        for (name, value) in changed {
            let last = changes.iter().rev().find(|c| &c.name == name);
            match last {
                Some(c) => t.push_str(&format!(
                    "* `{name}`: {value} ({}, {})\n",
                    wiki_line(&c.by, MAX_NAME),
                    day(c.at.date_naive())
                )),
                None => t.push_str(&format!("* `{name}`: {value}\n")),
            }
        }
        t.push_str("\n`!plenum settings` in the chat shows them all.\n");
    }
    t
}

/// The list `!top` alone answers.
pub fn list_text(
    plenum: &str,
    next: Option<MeetingKey>,
    for_next: &[(&Topic, Option<String>)],
    later: &[(&Topic, Option<String>)],
) -> String {
    let line = |(t, link): &(&Topic, Option<String>)| {
        let number = match link {
            Some(link) => format!("[#{}]({link})", t.number),
            None => format!("#{}", t.number),
        };
        format!(
            "• {number} {} ({}) · {}",
            md(&t.title),
            md(&t.by),
            kind_words(t)
        )
    };
    let when = match next {
        Some(key) => format!("on {}", short_meeting(key)),
        None => "(no meeting in the calendar yet)".to_owned(),
    };
    let mut s = if for_next.is_empty() {
        format!("📋 **{}** {when}: no topics yet.", md(plenum))
    } else {
        format!(
            "📋 **{}** {when}:\n{}",
            md(plenum),
            for_next.iter().map(line).collect::<Vec<_>>().join("\n")
        )
    };
    if !later.is_empty() {
        s.push_str(&format!(
            "\n\nLater:\n{}",
            later.iter().map(line).collect::<Vec<_>>().join("\n")
        ));
    }
    s.push_str("\n\nNew topic: `!top <topic>` (`!top decide: …` to decide).");
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn topic(n: u32, title: &str) -> Topic {
        Topic {
            number: n,
            title: title.into(),
            by: "Anna".into(),
            by_user: Some("@anna:house.org".into()),
            at: Utc::now(),
            ..Default::default()
        }
    }

    #[test]
    fn a_submission_is_its_first_line_and_maybe_more() {
        let s = parse_submission("Washing machine broken").unwrap();
        assert_eq!(s.title, "Washing machine broken");
        assert!(!s.decide && s.details.is_empty() && s.proposal.is_none());

        let s = parse_submission(
            "decide: New washing machine\nThe old one leaks.\nSince October.\n**Vorschlag:** buy one\nup to 600 €",
        )
        .unwrap();
        assert!(s.decide);
        assert_eq!(s.title, "New washing machine");
        assert_eq!(s.details, "The old one leaks.\nSince October.");
        assert_eq!(s.proposal.as_deref(), Some("buy one\nup to 600 €"));

        assert!(parse_submission("Entscheiden: Ruhezeiten").unwrap().decide);
        assert!(parse_submission("Decision: quiet hours").unwrap().decide);
        // "decided" is no prefix, a colon is needed
        assert!(!parse_submission("decided things").unwrap().decide);
        assert_eq!(parse_submission("   \n  "), None);
        assert_eq!(parse_submission("decide:   "), None);
        let long = "x".repeat(500);
        assert_eq!(
            parse_submission(&long).unwrap().title.chars().count(),
            MAX_TITLE
        );
    }

    #[test]
    fn a_proposal_in_the_thread_starts_with_its_label() {
        assert_eq!(proposal_in("Proposal: buy one").as_deref(), Some("buy one"));
        assert_eq!(
            proposal_in("vorschlag: kaufen\nbis 600 €").as_deref(),
            Some("kaufen\nbis 600 €")
        );
        assert_eq!(proposal_in("I'd propose: buy one"), None);
        assert_eq!(proposal_in("Proposal:"), None);
    }

    #[test]
    fn names_lose_what_hides_or_shouts() {
        assert_eq!(person_name("An\u{202E}na\u{200B}"), "Anna");
        assert_eq!(person_name("\u{200B}"), "someone");
        assert_eq!(person_name(&"x".repeat(100)).chars().count(), MAX_NAME);
        assert_eq!(
            md("[vote](https://evil) *now*"),
            "\\[vote\\]\\(https://evil\\) \\*now\\*"
        );
    }

    #[test]
    fn nothing_from_people_becomes_a_decision_task_or_heading_in_the_minutes() {
        let mut t = topic(12, "## Minutes");
        t.details =
            "Decision: we buy it\n- [ ] Task: Ben buys\n### #13 fake\n1. first\n> quote".into();
        t.proposal = Some("Task: nobody\n[x] done".into());
        t.by = "[Anna](https://evil)".into();
        let text = format!(
            "## Agenda\n\n{}\n\n## Minutes\n\n{}\n## Decisions and tasks\n",
            agenda_line(&t),
            section(&t, Some("https://matrix.to/#/!r:h/$e"))
        );
        let found = protocol::extract(&text);
        assert!(found.decisions.is_empty(), "{text}");
        assert!(found.tasks.is_empty(), "{text}");
        let secs = sections(&text);
        assert_eq!(secs.keys().copied().collect::<Vec<_>>(), vec![12], "{text}");
        assert!(!secs[&12].written, "{text}");
        assert!(text.contains("(Anna)(https://evil)"), "{text}");
    }

    #[test]
    fn a_section_says_what_came_of_a_topic() {
        let md = "## Agenda\n\n* #12 Washing machine (Anna) · to decide\n\n## Minutes\n\n\
                  ### #12 Washing machine\n\n> Anna, to decide\n\n↳ Discussion in the chat: [#12](x)\n\n\
                  We talked.\n\nDecision: we buy one\n\n- [ ] Ben asks for offers\n\n\
                  ### #13 Garden\n\n> Dana, to talk about\n\n\
                  ### \\#14 Escaped\n\nNotes.\n\n\\### #15 not a heading\n\n#### a sub-heading of 14\n\nmore\n\n\
                  ## Decisions and tasks\n\nDecision: general\n";
        let s = sections(md);
        assert_eq!(s[&12].decisions, vec!["we buy one"]);
        assert_eq!(s[&12].tasks, vec!["Ben asks for offers"]);
        assert!(s[&12].written);
        assert!(!s[&13].written);
        assert!(s[&13].decisions.is_empty());
        assert!(s[&14].written);
        assert!(!s.contains_key(&15));
        assert!(has_agenda_line(md, 12) && !has_agenda_line(md, 1));
        assert!(has_section(md, 14) && !has_section(md, 1));
        // a heading that only starts with a number is no topic's
        assert!(sections("### #12a x\n\ntext").is_empty());
        assert!(sections("### 12 x\n\ntext").is_empty());
    }

    #[test]
    fn sections_go_to_the_marker_or_the_end_of_the_minutes() {
        let t = topic(1, "First");
        let u = topic(2, "Second");
        let template =
            "## Agenda\n\n➕ x\n\n## Minutes\n\nWrite here.\n\n## Decisions and tasks\n\nDo.\n";
        let one = insert_section(template, &section(&t, None));
        let two = insert_section(&one, &section(&u, None));
        let first = two.find("### #1 First").unwrap();
        let second = two.find("### #2 Second").unwrap();
        let marker = two.find(SECTIONS_MARKER).unwrap();
        let decisions = two.find("## Decisions and tasks").unwrap();
        assert!(two.find("Write here.").unwrap() < first);
        assert!(
            first < second && second < marker && marker < decisions,
            "{two}"
        );
        // with the marker in the template
        let with = format!("## Minutes\n\n{SECTIONS_MARKER}\n\nEnd.\n");
        let placed = insert_section(&with, &section(&t, None));
        assert!(placed.find("### #1").unwrap() < placed.find(SECTIONS_MARKER).unwrap());
        assert_eq!(placed.matches(SECTIONS_MARKER).count(), 1);
        // without a minutes section: at the end under "## Topics"
        let bare = insert_section("Just text.", &section(&t, None));
        assert!(bare.contains("## Topics\n\n### #1 First"), "{bare}");
        assert!(bare.trim_end().ends_with(SECTIONS_MARKER));
    }

    #[test]
    fn the_card_follows_the_topic() {
        let mut t = topic(12, "Washing machine");
        let key: MeetingKey = "2026-11-15 19:00".parse().unwrap();
        let c = card(&t, Target::Meeting(key), true);
        assert!(
            c.contains("#12 Washing machine") && c.contains("to talk about"),
            "{c}"
        );
        assert!(c.contains("Sun 15 Nov, 19:00"), "{c}");
        assert!(c.contains("tap 🤝 to make it a topic to decide"), "{c}");
        assert!(c.contains("you hear what came of it"), "{c}");
        assert!(!card(&t, Target::Meeting(key), false).contains("you hear"));
        t.tap_decide = true;
        t.short_notice = true;
        let c = card(&t, Target::Next, true);
        assert!(c.contains("to decide") && c.contains("Short notice"), "{c}");
        assert!(c.contains("not in the calendar yet"), "{c}");
        assert!(c.contains("“Proposal: …”"), "{c}");
        t.history.push(Outcome {
            meeting: "m".into(),
            date: key.date(),
            result: Result::Decided,
            link: String::new(),
            confidential: false,
            told: true,
        });
        t.close(Status::Done, Utc::now());
        let c = card(&t, Target::Next, true);
        assert!(
            c.contains("Decided at the plenum on 15 Nov. The thread has more."),
            "{c}"
        );
        // a confidential meeting's: not even that it was decided
        t.history.last_mut().unwrap().confidential = true;
        let c = card(&t, Target::Next, true);
        assert!(
            c.contains("Dealt with at the plenum on 15 Nov") && !c.contains("Decided"),
            "{c}"
        );
        // `!top #12 done`: nothing came of a meeting
        t.history.clear();
        assert!(card(&t, Target::Next, true).contains("Marked as dealt with"));
        t.status = Status::Withdrawn;
        assert!(card(&t, Target::Next, true).contains("Withdrawn"));
    }

    #[test]
    fn the_agenda_puts_decisions_first_and_counts_nothing() {
        let mut a = topic(11, "Garden day");
        let mut b = topic(12, "Washing machine");
        b.decide = true;
        b.proposal = Some("buy a new one".into());
        a.carried = 1;
        a.history.push(Outcome {
            meeting: "m".into(),
            date: NaiveDate::from_ymd_opt(2026, 11, 1).unwrap(),
            result: Result::NotReached,
            link: String::new(),
            confidential: false,
            told: true,
        });
        let text = agenda(&[
            AgendaItem {
                topic: &a,
                link: None,
            },
            AgendaItem {
                topic: &b,
                link: Some("L".into()),
            },
        ]);
        let decide = text.find("To decide").unwrap();
        let talk = text.find("To talk about").unwrap();
        assert!(decide < talk, "{text}");
        assert!(
            text.contains("[#12](L) Washing machine (Anna): proposal: buy a new one"),
            "{text}"
        );
        assert!(
            text.contains("#11 Garden day (Anna) · again, not reached on 1 Nov"),
            "{text}"
        );
        assert!(!text.contains('👍'));
        assert_eq!(agenda(&[]), "");
    }

    #[test]
    fn outcomes_follow_what_may_be_shared() {
        let t = topic(12, "Washing machine");
        let d = NaiveDate::from_ymd_opt(2026, 11, 15).unwrap();
        let decisions = vec!["we buy one".to_owned()];
        let tasks = vec!["Ben asks for offers".to_owned()];
        let full = outcome_text(
            &t,
            Result::Decided,
            d,
            &decisions,
            &tasks,
            "L",
            Publication::Full,
            None,
            true,
        )
        .unwrap();
        assert!(
            full.contains("we buy one") && full.contains("Ben asks") && full.contains("L"),
            "{full}"
        );
        let link = outcome_text(
            &t,
            Result::Decided,
            d,
            &decisions,
            &tasks,
            "L",
            Publication::Link,
            None,
            true,
        )
        .unwrap();
        assert!(
            !link.contains("we buy one") && link.contains("Minutes: L"),
            "{link}"
        );
        let secret = outcome_text(
            &t,
            Result::Decided,
            d,
            &decisions,
            &tasks,
            "L",
            Publication::Confidential,
            None,
            true,
        )
        .unwrap();
        assert!(
            !secret.contains("we buy") && !secret.contains('L'),
            "{secret}"
        );
        assert!(outcome_text(
            &t,
            Result::Unknown,
            d,
            &[],
            &[],
            "L",
            Publication::Full,
            None,
            true
        )
        .is_none());
        let next: MeetingKey = "2026-11-29 19:00".parse().unwrap();
        let later = outcome_text(
            &t,
            Result::NotReached,
            d,
            &[],
            &[],
            "L",
            Publication::Full,
            Some(next),
            true,
        )
        .unwrap();
        assert!(
            later.contains("not reached on 15 Nov") && later.contains("29 Nov"),
            "{later}"
        );
    }

    #[test]
    fn texts_are_cleared_and_old_topics_dropped() {
        let now = Utc::now();
        let mut area = Area::default();
        let mut old = topic(1, "Old");
        old.details = "secret".into();
        old.close(Status::Done, now - chrono::Duration::days(40));
        let mut ancient = topic(2, "Ancient");
        ancient.close(Status::Withdrawn, now - chrono::Duration::days(200));
        let open = topic(3, "Open");
        area.topics.insert(1, old);
        area.topics.insert(2, ancient);
        area.topics.insert(3, open);
        area.prune(now);
        assert!(area.topics[&1].details.is_empty());
        assert!(!area.topics.contains_key(&2));
        assert!(area.topics.contains_key(&3));
    }

    #[test]
    fn a_topic_is_found_by_its_messages() {
        let mut area = Area::default();
        let mut t = topic(4, "x");
        t.source = Some("$src".into());
        t.card_event = Some("$card".into());
        area.topics.insert(4, t);
        assert_eq!(area.by_event("$src"), Some(4));
        assert_eq!(area.by_event("$card"), Some(4));
        assert_eq!(area.by_event("$other"), None);
    }
}
