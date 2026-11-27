//! Minutes posted in the chat, read as text (docs/CHAT-MINUTES.md): whether a message is
//! minutes, which day it names, the meeting it belongs to, its copy for the wiki, the labelled
//! lines (decisions, tasks, postponed topics, done tasks) and the bot's texts about it. Pure
//! functions; the flows are in `chat_flow.rs`.
//!
//! The labels here are for chat minutes only. The wiki's own readers (`protocol::extract`,
//! `page::topics`) keep their smaller set: a wider one would read existing pages differently.

use chrono::{Datelike, Duration, NaiveDate, NaiveDateTime};
use chrono_tz::Tz;

use crate::{
    protocol::{clean, strip_list_marker},
    schedule::MeetingKey,
    state::{Read, Taken},
};

/// The words a first line starts with (any case, after `#`, `*`, emoji and spaces).
const KEYWORDS: [&str; 4] = ["plenumsprotokoll", "protokoll", "minutes", "protocol"];

/// The first line of minutes is a title, not a paragraph.
const FIRST_LINE_MAX: usize = 80;

/// Minutes have more than a title: this many further lines, or this many characters.
const MORE_LINES: usize = 3;
const MORE_CHARS: usize = 300;

/// Without a day in it, minutes belong to the latest meeting that started at most this long ago.
pub const LOOK_BACK_DAYS: i64 = 21;

/// A day without a year is the latest such day at most this long ago.
const DAY_WITHOUT_YEAR_DAYS: i64 = 60;

/// One entry at most this long.
const ENTRY_MAX: usize = 500;

/// What a labelled line is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Decision,
    Task,
    Postponed,
    Done,
}

/// The labels, German and English (docs/CHAT-MINUTES.md). A word is added here only.
const LABELS: [(Kind, &[&str]); 4] = [
    (
        Kind::Decision,
        &[
            "decision",
            "decisions",
            "decided",
            "resolution",
            "resolved",
            "agreed",
            "agreement",
            "consensus",
            "beschluss",
            "beschlüsse",
            "beschlossen",
            "entscheidung",
            "entscheidungen",
            "entschieden",
            "einigung",
            "konsens",
        ],
    ),
    (
        Kind::Task,
        &[
            "task",
            "tasks",
            "to do",
            "to-do",
            "todo",
            "todos",
            "action",
            "actions",
            "action item",
            "action items",
            "aufgabe",
            "aufgaben",
            "zu tun",
            "zu erledigen",
            "auftrag",
            "aufträge",
        ],
    ),
    (
        Kind::Postponed,
        &[
            "postponed",
            "deferred",
            "adjourned",
            "tabled",
            "next time",
            "next meeting",
            "carried over",
            "not reached",
            "vertagt",
            "verschoben",
            "nächstes mal",
            "nächstes plenum",
            "nicht geschafft",
            "offen geblieben",
        ],
    ),
    (
        Kind::Done,
        &[
            "done",
            "completed",
            "finished",
            "erledigt",
            "fertig",
            "abgeschlossen",
        ],
    ),
];

/// The message as typed: without a reply's quoted fallback.
fn own_text(body: &str) -> &str {
    mxbot_common::format::strip_reply_fallback(body)
}

/// The line without what comes before its first letter or digit (`#`, `*`, emoji, spaces).
fn lead(line: &str) -> &str {
    line.trim_start_matches(|c: char| !c.is_alphanumeric())
}

/// Whether `lower` starts with `word` as a whole word.
fn starts_with_word(lower: &str, word: &str) -> bool {
    lower
        .strip_prefix(word)
        .is_some_and(|rest| !rest.starts_with(char::is_alphabetic))
}

fn first_line(body: &str) -> Option<&str> {
    own_text(body)
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
}

/// Whether a message is minutes by itself (CHAT-MINUTES.md §3): its first line starts with one of the
/// [`KEYWORDS`] and is short, and more follows.
pub fn is_minutes(body: &str) -> bool {
    let text = own_text(body);
    let mut lines = text.lines().map(str::trim).filter(|l| !l.is_empty());
    let Some(first) = lines.next() else {
        return false;
    };
    if first.chars().count() > FIRST_LINE_MAX {
        return false;
    }
    let lower = lead(first).to_lowercase();
    if !KEYWORDS.iter().any(|k| starts_with_word(&lower, k)) {
        return false;
    }
    lines.count() >= MORE_LINES || text.trim().chars().count() >= MORE_CHARS
}

/// Whether the first line names a plenum (by its key or its name), for a room with several.
pub fn names_plenum(body: &str, key: &str, name: &str) -> bool {
    let Some(first) = first_line(body) else {
        return false;
    };
    let lower = first.to_lowercase();
    let words: Vec<&str> = lower
        .split(|c: char| c.is_whitespace() || c == ',' || c == ':')
        .collect();
    words.contains(&key.to_lowercase().as_str()) || lower.contains(&name.to_lowercase())
}

/// The text of `!minutes [key]` followed by a line break and the minutes: the minutes. Also
/// for an edit of such a message. Anything else as it is.
pub fn without_command<'a>(body: &'a str, keys: &[&str]) -> &'a str {
    let text = own_text(body);
    let Some((first, rest)) = text.split_once('\n') else {
        return text;
    };
    let mut words = first.split_whitespace();
    let is_command = words
        .next()
        .is_some_and(|w| w.eq_ignore_ascii_case("!minutes"))
        && words.all(|w| keys.contains(&w));
    if is_command {
        rest.trim_start_matches(['\r', '\n'])
    } else {
        text
    }
}

fn month(word: &str) -> Option<u32> {
    let w = word.trim_end_matches('.').to_lowercase();
    let months: [&[&str]; 12] = [
        &["jan", "januar", "january", "jänner"],
        &["feb", "februar", "february"],
        &["mar", "mär", "märz", "maerz", "march"],
        &["apr", "april"],
        &["mai", "may"],
        &["jun", "juni", "june"],
        &["jul", "juli", "july"],
        &["aug", "august"],
        &["sep", "sept", "september"],
        &["okt", "oct", "oktober", "october"],
        &["nov", "november"],
        &["dez", "dec", "dezember", "december"],
    ];
    (1..=12u32).find(|m| months[*m as usize - 1].contains(&w.as_str()))
}

/// A day in the message's first two lines (CHAT-MINUTES.md §4): `15.11.`, `15.11.2026`, `2026-11-15`,
/// `15. November`. Without a year the latest such day not after `today`; a day after `today`
/// is none.
pub fn day_in(body: &str, today: NaiveDate) -> Option<NaiveDate> {
    let lines: Vec<&str> = own_text(body)
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .take(2)
        .collect();
    let words: Vec<String> = lines
        .iter()
        .flat_map(|l| l.split_whitespace())
        .map(|w| {
            w.trim_matches(|c: char| !(c.is_alphanumeric() || c == '.' || c == '-'))
                .to_owned()
        })
        .collect();
    for (i, w) in words.iter().enumerate() {
        let found = match crate::commands::parse_when(std::slice::from_ref(w)) {
            Ok(Some(when)) => Some((when.day, when.month, when.year)),
            _ => {
                let day = w.trim_end_matches('.').parse::<u32>().ok();
                let m = words.get(i + 1).and_then(|n| month(n));
                day.zip(m).map(|(d, m)| {
                    let year = words.get(i + 2).and_then(|y| y.parse::<i32>().ok());
                    (d, m, year.filter(|y| (2000..=2100).contains(y)))
                })
            }
        };
        let Some((day, m, year)) = found else {
            continue;
        };
        let date = match year {
            Some(y) => NaiveDate::from_ymd_opt(y, m, day).filter(|d| *d <= today),
            None => [today.year(), today.year() - 1]
                .into_iter()
                .filter_map(|y| NaiveDate::from_ymd_opt(y, m, day))
                .find(|d| *d <= today && today - *d <= Duration::days(DAY_WITHOUT_YEAR_DAYS)),
        };
        if date.is_some() {
            return date;
        }
    }
    None
}

/// The meeting minutes belong to (CHAT-MINUTES.md §4), of `meetings` (ID, start; neither cancelled nor
/// skipped): on `day` the one that has started (the latest of them), else the first that
/// day; without a day the latest that started at most [`LOOK_BACK_DAYS`] ago. None: filed by
/// the day.
pub fn meeting_for(
    day: Option<NaiveDate>,
    meetings: &[(String, MeetingKey)],
    now: NaiveDateTime,
) -> Option<String> {
    let mut sorted: Vec<&(String, MeetingKey)> = meetings.iter().collect();
    sorted.sort_by(|a, b| (a.1, &a.0).cmp(&(b.1, &b.0)));
    match day {
        Some(day) => {
            let on: Vec<&&(String, MeetingKey)> =
                sorted.iter().filter(|(_, s)| s.date() == day).collect();
            on.iter()
                .rev()
                .find(|(_, s)| s.0 <= now)
                .or(on.first())
                .map(|(id, _)| id.clone())
        }
        None => sorted
            .iter()
            .rev()
            .find(|(_, s)| s.0 <= now && now - s.0 <= Duration::days(LOOK_BACK_DAYS))
            .map(|(id, _)| id.clone()),
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Block {
    Start,
    Blank,
    Text,
    List,
}

/// A link's target people may follow from the wiki.
fn followable(url: &str) -> bool {
    let lower = url.to_lowercase();
    ["http://", "https://", "mailto:"]
        .iter()
        .any(|s| lower.starts_with(s))
}

/// One line's links: mentions as their label, images as links, other schemes as text.
fn links(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    while let Some(open) = rest.find('[') {
        let image = open > 0 && rest[..open].ends_with('!');
        let (before, tail) = rest.split_at(open);
        let parsed = tail[1..].split_once("](").and_then(|(label, after)| {
            let end = after.find(')')?;
            let url = &after[..end];
            let ok = !label.contains(['[', ']', '\n']) && !url.contains(char::is_whitespace);
            ok.then(|| (label, url, &after[end + 1..]))
        });
        let Some((label, url, after)) = parsed else {
            out.push_str(before);
            out.push('[');
            rest = &tail[1..];
            continue;
        };
        out.push_str(if image {
            &before[..before.len() - 1]
        } else {
            before
        });
        if followable(url) {
            out.push_str(&format!("[{label}]({url})"));
        } else {
            out.push_str(label);
        }
        rest = after;
    }
    out.push_str(rest);
    out
}

fn is_list_line(line: &str) -> bool {
    let t = line.trim_start();
    t.starts_with('|') || strip_list_marker(t).1.len() != t.len()
}

/// The message's text as Markdown for its wiki copy (CHAT-MINUTES.md §2): a chat line
/// break is a line, so it becomes a paragraph break (list items and table rows stay
/// together); code blocks as they are; a line cannot open a notice; mentions keep their
/// label, images become links, links other than http, https and mailto become text.
pub fn to_wiki(body: &str) -> String {
    let mut out = String::new();
    let mut prev = Block::Start;
    let mut code = false;
    for line in own_text(body).trim().lines() {
        let line = line.trim_end();
        let fence = line.trim_start().starts_with("```") || line.trim_start().starts_with("~~~");
        if code {
            out.push('\n');
            out.push_str(line);
            if fence {
                code = false;
                prev = Block::Text;
            }
            continue;
        }
        if line.trim().is_empty() {
            if prev != Block::Start {
                prev = Block::Blank;
            }
            continue;
        }
        let kind = if is_list_line(line) {
            Block::List
        } else {
            Block::Text
        };
        let sep = match (prev, kind) {
            (Block::Start, _) => "",
            (Block::List, Block::List) => "\n",
            _ => "\n\n",
        };
        out.push_str(sep);
        if fence {
            out.push_str(line);
            code = true;
            prev = Block::Text;
            continue;
        }
        let mut converted = links(line);
        if converted.trim_start().starts_with(":::") {
            let at = converted.len() - converted.trim_start().len();
            converted.insert(at, '\\');
        }
        out.push_str(&converted);
        prev = kind;
    }
    if code {
        out.push_str("\n```");
    }
    out
}

/// `(kind, the text after the label)` of a line that starts with a label: "Beschluss: …"
/// (also bold, as a list item, after an emoji), or the label alone as a heading or in bold
/// (the start of a section; the text is empty then). A checkbox item is never a label line.
fn label_of(line: &str) -> Option<(Kind, String)> {
    let t = line.trim();
    let heading = t.starts_with('#');
    let t = t.trim_start_matches('#').trim_start();
    let (checkbox, t) = strip_list_marker(t);
    if checkbox.is_some() {
        return None;
    }
    let bold = t.trim_start().starts_with("**") || t.trim_start().starts_with("__");
    let t = lead(t);
    let mut labels: Vec<(Kind, &str)> = LABELS
        .iter()
        .flat_map(|(k, ls)| ls.iter().map(|l| (*k, *l)))
        .collect();
    labels.sort_by_key(|(_, l)| std::cmp::Reverse(l.chars().count()));
    for (kind, label) in labels {
        let n = label.chars().count();
        let head: String = t.chars().take(n).collect();
        if head.to_lowercase() != label {
            continue;
        }
        let after = t[head.len()..].trim_start_matches(['*', '_']);
        if let Some(rest) = after.trim_start().strip_prefix(':') {
            return Some((kind, clean(rest.trim_start_matches(['*', '_']))));
        }
        if after.trim().is_empty() && (heading || bold) {
            return Some((kind, String::new()));
        }
    }
    None
}

/// An entry's text, or none for what is no entry (empty, a lone "-", an ellipsis).
fn entry(text: &str) -> Option<String> {
    let t = clean(text);
    let t = t.trim();
    let blank = t
        .chars()
        .all(|c| matches!(c, '-' | '–' | '—' | '.' | '…' | '_' | ' '));
    if blank {
        return None;
    }
    Some(t.chars().take(ENTRY_MAX).collect())
}

/// What the labelled lines say (CHAT-MINUTES.md §5): one line ("Beschluss: …"), or a section (the label
/// alone, also as a heading, then its lines up to an empty line, the next label or a
/// heading). An open checkbox is a task, a ticked one done; an open one under "Done:" is
/// neither (the template's open tasks nobody ticked). Code blocks are not read.
pub fn read(body: &str) -> Read {
    let mut out = Read::default();
    let mut push = |kind: Kind, text: &str| {
        let Some(text) = entry(text) else {
            return;
        };
        let list = match kind {
            Kind::Decision => &mut out.decisions,
            Kind::Task => &mut out.tasks,
            Kind::Postponed => &mut out.postponed,
            Kind::Done => &mut out.done,
        };
        if !list.contains(&text) {
            list.push(text);
        }
    };
    let mut section: Option<Kind> = None;
    let mut code = false;
    for line in own_text(body).lines() {
        let t = line.trim();
        if t.starts_with("```") || t.starts_with("~~~") {
            code = !code;
            section = None;
            continue;
        }
        if code {
            continue;
        }
        if t.is_empty() {
            section = None;
            continue;
        }
        if let Some((kind, rest)) = label_of(t) {
            if rest.is_empty() {
                section = Some(kind);
            } else {
                push(kind, &rest);
                section = None;
            }
            continue;
        }
        if t.starts_with('#') {
            section = None;
            continue;
        }
        match strip_list_marker(t) {
            (Some(true), rest) => push(Kind::Done, rest),
            (Some(false), _) if section == Some(Kind::Done) => {}
            (Some(false), rest) => push(Kind::Task, rest),
            (None, rest) => {
                if let Some(kind) = section {
                    push(kind, rest);
                }
            }
        }
    }
    out
}

/// A text compared loosely: lower case, letters and digits as words, without a "(since …)".
fn loose(text: &str) -> String {
    let text = match text.rfind("(since ") {
        Some(i) => &text[..i],
        None => text,
    };
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Whether `a` and `b` say the same (one contains the other, loosely): a postponed line and
/// a topic's title.
pub fn same(a: &str, b: &str) -> bool {
    let (a, b) = (loose(a), loose(b));
    let (short, long) = if a.len() <= b.len() { (a, b) } else { (b, a) };
    short.chars().count() >= 4 && format!(" {long} ").contains(&format!(" {short} "))
}

/// A done line must be this many words long to close a task it is only a part of.
const DONE_WORDS: usize = 3;

/// Whether a done line closes an open task: the same text (as a ticked checkbox of the
/// template has it), or one a part of the other with at least [`DONE_WORDS`] words. A name
/// alone ("Done: Anna") closes nothing: it would close every task of that person.
pub fn closes(done: &str, task: &str) -> bool {
    let (done, task) = (loose(done), loose(task));
    if done.is_empty() || task.is_empty() {
        return false;
    }
    if done == task {
        return true;
    }
    let (short, long) = if done.len() <= task.len() {
        (done, task)
    } else {
        (task, done)
    };
    short.split(' ').count() >= DONE_WORDS && format!(" {long} ").contains(&format!(" {short} "))
}

/// A text's exact hash (FNV-1a over its bytes, stable: it is kept in minutes.json).
pub fn exact(text: &str) -> u64 {
    text.bytes().fold(0xcbf2_9ce4_8422_2325, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

/// Whether two tasks are the same task (loosely equal, the "(since …)" aside).
pub fn same_task(a: &str, b: &str) -> bool {
    let (a, b) = (loose(a), loose(b));
    !a.is_empty() && a == b
}

/// The one reminder that no minutes came for a meeting (`link`: its page, not in a
/// confidential meeting).
pub fn nudge(plenum: &str, start: MeetingKey, link: Option<&str>) -> String {
    let mut text = format!(
        "📝 No minutes for **{plenum}** on {} yet. Post them here as a message whose first line \
         starts with \"Minutes\" (or answer the message that has them with `!minutes`), or write \
         them on the page",
        crate::schedule::describe_meeting(start)
    );
    match link {
        Some(link) => text.push_str(&format!(": {link}")),
        None => text.push_str(" in the wiki."),
    }
    text
}

/// A name or a line from the chat as Markdown that stays text.
pub fn literal(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars().filter(|c| !c.is_control()) {
        if matches!(
            c,
            '\\' | '*' | '_' | '`' | '~' | '[' | ']' | '<' | '>' | '#' | '|'
        ) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

fn when(at: chrono::DateTime<chrono::Utc>, tz: Tz) -> String {
    at.with_timezone(&tz).format("%-d %b %Y, %H:%M").to_string()
}

/// The text of the wiki copy (CHAT-MINUTES.md §2): a notice with who posted it and how it is kept,
/// then the messages, each under its author and time when there are several.
pub fn copy_text(messages: &[&Taken], tz: Tz) -> String {
    let changed = |t: &Taken| {
        (t.text_at - t.posted_at >= Duration::minutes(1)).then(|| {
            let same_day = t.text_at.with_timezone(&tz).date_naive()
                == t.posted_at.with_timezone(&tz).date_naive();
            if same_day {
                t.text_at.with_timezone(&tz).format("%H:%M").to_string()
            } else {
                when(t.text_at, tz)
            }
        })
    };
    let mut text = String::from(":::info\n");
    match messages {
        [one] => {
            text.push_str(&format!(
                "Copy of the minutes {} posted in the chat on {}",
                literal(&one.sender_name),
                when(one.posted_at, tz)
            ));
            if let Some(at) = changed(one) {
                text.push_str(&format!(" (last change {at})"));
            }
            text.push('.');
        }
        several => text.push_str(&format!(
            "Copy of the minutes posted in the chat ({} messages).",
            several.len()
        )),
    }
    text.push_str(
        "\nTo change them, edit the message in the chat: the bot follows. If this copy is \
         edited here, the bot stops following.\n:::\n",
    );
    for t in messages {
        if messages.len() > 1 {
            text.push_str(&format!(
                "\n**{}, {}**",
                literal(&t.sender_name),
                when(t.posted_at, tz)
            ));
            if let Some(at) = changed(t) {
                text.push_str(&format!(" (last change {at})"));
            }
            text.push('\n');
        }
        text.push('\n');
        text.push_str(&to_wiki(&t.text));
        text.push('\n');
    }
    text
}

/// What a thread reply is about.
pub struct Receipt<'a> {
    pub plenum: &'a str,
    /// The meeting ("Sunday, 15 November 2026 at 19:00"), or None: filed by its day.
    pub meeting: Option<String>,
    pub day: NaiveDate,
    /// The copy's link (None in a confidential meeting).
    pub link: Option<String>,
    /// What was read from the message this answers.
    pub read: &'a Read,
    /// The house reads the copy (a kitchen's collection).
    pub house_reads: bool,
    pub withdrawn: bool,
}

/// The thread reply to a message filed as minutes (CHAT-MINUTES.md §6). It repeats only what the
/// message says, never anything from the wiki.
pub fn receipt(r: &Receipt) -> String {
    let what = match &r.meeting {
        Some(m) => format!("the minutes of {}, {m}", r.plenum),
        None => format!(
            "minutes of {} by their day, {} (no meeting in the calendar)",
            r.plenum,
            r.day.format("%A %-d %B %Y")
        ),
    };
    if r.withdrawn {
        return format!(
            "↩️ Withdrawn: no longer {what}. Its decisions and tasks are taken out again."
        );
    }
    let mut text = format!("📥 Filed as {what}");
    match &r.link {
        Some(link) => text.push_str(&format!(": {link}")),
        None => text.push('.'),
    }
    if r.house_reads {
        text.push_str(" (the whole house can read it in the wiki)");
    }
    let lines = |label: &str, items: &[String]| {
        (!items.is_empty()).then(|| {
            format!(
                "{label} ({}): {}",
                items.len(),
                items
                    .iter()
                    .map(|i| literal(i))
                    .collect::<Vec<_>>()
                    .join("; ")
            )
        })
    };
    let read: Vec<String> = [
        lines("Decisions", &r.read.decisions),
        lines("Tasks", &r.read.tasks),
        lines("Postponed", &r.read.postponed),
        lines("Done", &r.read.done),
    ]
    .into_iter()
    .flatten()
    .collect();
    if read.is_empty() {
        text.push_str(
            "\n\nNo decisions or tasks recognised. Lines that start with Decision:, Task: or \
             Postponed: (also Beschluss:, Aufgabe:, Vertagt:) are tracked.",
        );
    } else {
        for line in read {
            text.push_str("\n\n");
            text.push_str(&line);
        }
    }
    text.push_str(
        "\n\nEdit your message and I follow. Not the minutes? Answer with `!minutes undo`.",
    );
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
Protokoll Hausplenum 15.11.
Anwesend: Anna, Ben, Cara

Fahrradkeller: zu voll, jedes Rad bekommt ein Namensschild.
Beschluss: Räder ohne Schild kommen im Januar in die Werkstatt.
Aufgabe: Anna besorgt Schilder bis 30.11.
Vertagt: Werkzeugregal";

    const PLAIN: &str = "\
Minutes of the house plenum
We talked about the garden and the bikes.
The garden party is in June, Ben asks the neighbours.
Nobody wanted to talk about the cellar.";

    fn today() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 11, 27).unwrap()
    }

    fn day(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn the_sample_and_a_plain_message_are_minutes() {
        assert!(is_minutes(SAMPLE));
        assert!(is_minutes(PLAIN));
        let r = read(SAMPLE);
        assert_eq!(
            r.decisions,
            ["Räder ohne Schild kommen im Januar in die Werkstatt."]
        );
        assert_eq!(r.tasks, ["Anna besorgt Schilder bis 30.11."]);
        assert_eq!(r.postponed, ["Werkzeugregal"]);
        assert!(r.done.is_empty());
        assert_eq!(read(PLAIN), Read::default());
    }

    #[test]
    fn what_is_not_minutes() {
        for body in [
            "Protokoll kommt morgen",
            "Protokoll kommt morgen\nversprochen",
            "Protokollant gesucht\na\nb\nc",
            &format!(
                "Gestern im Plenum hat Anna das Protokoll geschrieben.\n{}",
                "a\n".repeat(5)
            ),
            &format!("Protokoll {}\na\nb\nc", "x".repeat(80)),
            "",
        ] {
            assert!(!is_minutes(body), "{body}");
        }
        for body in [
            "## 📝 **Plenumsprotokoll** 15.11.\na\nb\nc",
            "MINUTES house plenum\na\nb\nc",
            "Protocol: kitchen\na\nb\nc",
            &format!("Protokoll\n{}", "Ein langer Absatz. ".repeat(20)),
            "> <@ben:example.org> where are the minutes?\n\nProtokoll\na\nb\nc",
        ] {
            assert!(is_minutes(body), "{body}");
        }
    }

    #[test]
    fn every_label_is_read_in_both_forms() {
        for (kind, labels) in LABELS {
            for label in labels {
                let cap: String = label
                    .chars()
                    .enumerate()
                    .map(|(i, c)| {
                        if i == 0 {
                            c.to_uppercase().next().unwrap()
                        } else {
                            c
                        }
                    })
                    .collect();
                for line in [
                    format!("{cap}: the thing"),
                    format!("- {cap}: the thing"),
                    format!("**{cap}:** the thing"),
                    format!("**{cap}**: the thing"),
                    format!("{}: the thing", label.to_uppercase()),
                    format!("✅ {cap}: the thing"),
                    format!("{cap}:\n- the thing"),
                    format!("## {cap}\n- the thing"),
                    format!("**{cap}**\nthe thing"),
                    format!("{cap}:\nthe thing"),
                ] {
                    let r = read(&line);
                    let got = match kind {
                        Kind::Decision => &r.decisions,
                        Kind::Task => &r.tasks,
                        Kind::Postponed => &r.postponed,
                        Kind::Done => &r.done,
                    };
                    assert_eq!(got, &strings(&["the thing"]), "{line:?}: {r:?}");
                }
            }
        }
    }

    #[test]
    fn a_section_ends_at_an_empty_line_a_label_or_a_heading() {
        let r = read("Beschlüsse:\n- a\n- b\n\n- c\nAufgaben:\n- d\n## Garten\n- e");
        assert_eq!(r.decisions, ["a", "b"]);
        assert_eq!(r.tasks, ["d"]);
        let r = read("## Decisions\n1. one\n2. two\nTasks:\n* three");
        assert_eq!(r.decisions, ["one", "two"]);
        assert_eq!(r.tasks, ["three"]);
    }

    #[test]
    fn what_is_no_entry() {
        let r = read("Ergebnis: alles gut\nResult: fine\nWir haben Beschluss: keiner\nDecisions:\n-\n\nTasks:\n- \n\nPostponed:\n–\nDone:\n- [ ] Ben: write to the landlord (since 1 Nov 2026)");
        assert_eq!(r, Read::default(), "{r:?}");
        let r = read("Beschluss:\n\nAufgabe:");
        assert_eq!(r, Read::default());
        let r = read("```\nBeschluss: in code\n```\nTask: real");
        assert!(r.decisions.is_empty());
        assert_eq!(r.tasks, ["real"]);
    }

    #[test]
    fn checkboxes_are_tasks_and_done() {
        let r = read("- [ ] Anna: Schilder\n- [x] Ben: Tonne kaufen\nDone:\n- [x] Cara: Liste\n- [ ] Dirk: offen");
        assert_eq!(r.tasks, ["Anna: Schilder"]);
        assert_eq!(r.done, ["Ben: Tonne kaufen", "Cara: Liste"]);
        let r = read("Erledigt: Ben: Tonne kaufen");
        assert_eq!(r.done, ["Ben: Tonne kaufen"]);
    }

    #[test]
    fn a_day_in_the_first_two_lines() {
        let d = |body: &str| day_in(body, today());
        assert_eq!(d("Protokoll 15.11."), Some(day(2026, 11, 15)));
        assert_eq!(d("Protokoll 15.11"), Some(day(2026, 11, 15)));
        assert_eq!(d("Protokoll vom 15.11.2026"), Some(day(2026, 11, 15)));
        assert_eq!(d("Protokoll (2026-11-15)"), Some(day(2026, 11, 15)));
        assert_eq!(
            d("Protokoll\nPlenum am 15. November"),
            Some(day(2026, 11, 15))
        );
        assert_eq!(d("Minutes 15 Nov 2026"), Some(day(2026, 11, 15)));
        assert_eq!(d("Minutes, 3. März:"), None, "more than 60 days back");
        assert_eq!(d("Protokoll\nzweite Zeile\n15.11."), None, "only two lines");
        assert_eq!(d("Protokoll 30.11."), None, "in the future");
        assert_eq!(d("Protokoll 15.12.2026"), None, "in the future");
        assert_eq!(d("Protokoll um 19.00 Uhr"), None);
        assert_eq!(d("Protokoll"), None);
        // early in January a day of December is last year's
        let jan = day(2027, 1, 3);
        assert_eq!(day_in("Protokoll 29.12.", jan), Some(day(2026, 12, 29)));
    }

    #[test]
    fn the_meeting_minutes_belong_to() {
        let m = |id: &str, s: &str| (id.to_owned(), s.parse::<MeetingKey>().unwrap());
        let meetings = vec![
            m("nov1", "2026-11-01 19:00"),
            m("nov15a", "2026-11-15 10:00"),
            m("nov15b", "2026-11-15 19:00"),
            m("dec6", "2026-12-06 19:00"),
        ];
        let now = |s: &str| s.parse::<MeetingKey>().unwrap().0;
        let pick = |d, n| meeting_for(d, &meetings, now(n));
        // by the day; two that day: the latest that started, else the first
        assert_eq!(
            pick(Some(day(2026, 11, 1)), "2026-11-16 10:00").as_deref(),
            Some("nov1")
        );
        assert_eq!(
            pick(Some(day(2026, 11, 15)), "2026-11-16 10:00").as_deref(),
            Some("nov15b")
        );
        assert_eq!(
            pick(Some(day(2026, 11, 15)), "2026-11-15 12:00").as_deref(),
            Some("nov15a")
        );
        assert_eq!(pick(Some(day(2026, 11, 14)), "2026-11-16 10:00"), None);
        // without a day: the latest that started, at most 21 days back
        assert_eq!(pick(None, "2026-11-16 10:00").as_deref(), Some("nov15b"));
        assert_eq!(pick(None, "2026-12-06 18:00").as_deref(), Some("nov15b"));
        assert_eq!(pick(None, "2026-12-07 10:00").as_deref(), Some("dec6"));
        assert_eq!(pick(None, "2026-12-27 18:00").as_deref(), Some("dec6"));
        assert_eq!(
            pick(None, "2026-12-28 20:00"),
            None,
            "no meeting in 21 days"
        );
        assert_eq!(pick(None, "2026-10-30 20:00"), None);
    }

    #[test]
    fn the_wiki_copy_keeps_the_lines_and_nothing_breaks_the_page() {
        assert_eq!(to_wiki("Protokoll\nAnna\nBen"), "Protokoll\n\nAnna\n\nBen");
        assert_eq!(
            to_wiki("Aufgaben:\n- a\n- [ ] b\n  - c\n\nEnde"),
            "Aufgaben:\n\n- a\n- [ ] b\n  - c\n\nEnde"
        );
        assert_eq!(
            to_wiki("Code:\n```\nx\n\ny\n```\nnach"),
            "Code:\n\n```\nx\n\ny\n```\n\nnach"
        );
        assert_eq!(
            to_wiki(":::warning\nboo\n:::"),
            "\\:::warning\n\nboo\n\n\\:::"
        );
        assert_eq!(
            to_wiki("Bild ![Plan](https://example.org/p.png) und ![x](mxc://example.org/abc)"),
            "Bild [Plan](https://example.org/p.png) und x"
        );
        assert_eq!(
            to_wiki("@[Anna](mention://1/user/2) und [klick](javascript:alert) [web](https://example.org) [mail](mailto:a@example.org)"),
            "@Anna und klick [web](https://example.org) [mail](mailto:a@example.org)"
        );
        assert_eq!(
            to_wiki("> <@ben:example.org> wo?\n\nProtokoll\nx"),
            "Protokoll\n\nx"
        );
        assert_eq!(
            to_wiki("| a | b |\n|---|---|\n| 1 | 2 |"),
            "| a | b |\n|---|---|\n| 1 | 2 |"
        );
    }

    #[test]
    fn a_command_line_before_the_minutes_goes() {
        assert_eq!(
            without_command("!minutes\nProtokoll\nx", &[]),
            "Protokoll\nx"
        );
        assert_eq!(
            without_command("!minutes kitchen\nProtokoll", &["kitchen"]),
            "Protokoll"
        );
        assert_eq!(without_command("!minutes done\nx", &[]), "!minutes done\nx");
        assert_eq!(without_command("Protokoll\nx", &[]), "Protokoll\nx");
    }

    #[test]
    fn a_plenum_named_in_the_first_line() {
        assert!(names_plenum(
            "Protokoll kitchen-1 15.11.\nx",
            "kitchen-1",
            "Kitchen one"
        ));
        assert!(names_plenum(
            "Protokoll Kitchen One\nx",
            "kitchen-1",
            "Kitchen one"
        ));
        assert!(!names_plenum(
            "Protokoll\nkitchen-1",
            "kitchen-1",
            "Kitchen one"
        ));
    }

    #[test]
    fn done_lines_and_postponed_topics_match_loosely() {
        assert!(same(
            "Ben: Tonne kaufen",
            "Ben: Tonne kaufen (since 1 Nov 2026)"
        ));
        assert!(same("tonne kaufen", "Ben: **Tonne** kaufen"));
        assert!(same("Werkzeugregal", "Werkzeugregal im Keller"));
        assert!(!same("Regal", "Werkzeugregal"));
        assert!(!same("ab", "ab cd"));
    }

    #[test]
    fn a_done_line_closes_the_task_it_names_and_no_other() {
        let task = "Anna: order the name tags (since 1 Nov 2026)";
        // the template's ticked checkbox, with and without the day
        assert!(closes("Anna: order the name tags (since 1 Nov 2026)", task));
        assert!(closes("anna order the name tags", task));
        // a part of it that is long enough, either way round
        assert!(closes("order the name tags", task));
        assert!(closes(
            "Anna: order the name tags for the bike cellar",
            task
        ));
        // a name or a word alone would close every task that has it
        assert!(!closes("Anna", task));
        assert!(!closes("name tags", task));
        assert!(!closes("Ben: order the name tags", task));
        assert!(!closes("", task));
    }

    fn taken(name: &str, posted: &str, changed: &str, text: &str) -> Taken {
        Taken {
            event: "$e".into(),
            thread: "$e".into(),
            sender: "@anna:example.org".into(),
            sender_name: name.into(),
            taken_by: None,
            posted_at: posted.parse().unwrap(),
            text: text.into(),
            text_at: changed.parse().unwrap(),
            withdrawn: None,
            reaction: Default::default(),
            reaction_event: None,
            receipt: Default::default(),
            receipt_message: None,
        }
    }

    #[test]
    fn the_copy_says_who_posted_it_and_how_it_is_kept() {
        let tz: Tz = "Europe/Berlin".parse().unwrap();
        let a = taken(
            "Anna",
            "2026-11-16T09:12:00Z",
            "2026-11-16T09:40:00Z",
            SAMPLE,
        );
        let text = copy_text(&[&a], tz);
        assert!(text.starts_with(
            ":::info\nCopy of the minutes Anna posted in the chat on 16 Nov 2026, 10:12 (last change 10:40).\n"
        ), "{text}");
        assert!(
            text.contains(":::\n\nProtokoll Hausplenum 15.11.\n\nAnwesend: Anna, Ben, Cara\n\n")
        );
        let b = taken(
            "Ben_x",
            "2026-11-17T09:00:00Z",
            "2026-11-17T09:00:00Z",
            "Teil 2\nmehr",
        );
        let text = copy_text(&[&a, &b], tz);
        assert!(text.contains("(2 messages)"));
        assert!(text.contains("**Anna, 16 Nov 2026, 10:12** (last change 10:40)\n\nProtokoll"));
        assert!(
            text.contains("**Ben\\_x, 17 Nov 2026, 10:00**\n\nTeil 2\n\nmehr"),
            "{text}"
        );
    }

    #[test]
    fn the_reply_says_what_was_read_and_nothing_else() {
        let r = read(SAMPLE);
        let base = Receipt {
            plenum: "House plenum",
            meeting: Some("Sunday, 15 November 2026 at 19:00".into()),
            day: day(2026, 11, 15),
            link: Some("https://wiki.example.org/doc/x".into()),
            read: &r,
            house_reads: false,
            withdrawn: false,
        };
        let text = receipt(&base);
        assert!(text.starts_with("📥 Filed as the minutes of House plenum, Sunday, 15 November 2026 at 19:00: https://wiki.example.org/doc/x"), "{text}");
        assert!(text.contains("Decisions (1): Räder ohne Schild"));
        assert!(text.contains("Tasks (1): Anna besorgt"));
        assert!(text.contains("Postponed (1): Werkzeugregal"));
        assert!(text.ends_with("`!minutes undo`."));
        let none = Read::default();
        let text = receipt(&Receipt {
            link: None,
            read: &none,
            house_reads: true,
            ..base
        });
        assert!(!text.contains("https://"), "{text}");
        assert!(text.contains("No decisions or tasks recognised"));
        assert!(text.contains("the whole house can read it"));
        let text = receipt(&Receipt {
            meeting: None,
            read: &none,
            link: None,
            withdrawn: true,
            plenum: "House plenum",
            day: day(2026, 11, 15),
            house_reads: false,
        });
        assert!(text.starts_with("↩️ Withdrawn"), "{text}");
    }
}
