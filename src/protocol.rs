//! The protocol (the minutes) as Markdown: the template, agenda items from the chat, and what
//! is read back at the end (decisions and tasks). Pure text functions; Outline stores Markdown
//! and gives it back in its own spelling (`*` for bullets, `- [ ]` for checkboxes).
//!
//! The bot writes English (2026-10-22). Protocols and templates from before, in German, are
//! still read: their marker, headings, labels and placeholders are known too.

use crate::{
    config::Plenum,
    schedule::{describe_day, describe_meeting, weekday_de, MeetingKey},
    state::{Carried, Top},
};
use chrono::Datelike;

/// The line agenda items from the chat are put above. It is plain text on purpose: Outline
/// keeps it exactly as it is, while formatting would come back in another spelling.
pub const TOP_MARKER: &str = "➕ New items from the chat (!top) go here.";

/// Every marker the bot knows: [`TOP_MARKER`], and the German one of older protocols.
pub const TOP_MARKERS: [&str; 2] = [
    TOP_MARKER,
    "➕ Neue Punkte kommen mit !top im Chat hier dazu.",
];

/// The template a plenum starts with when its collection has none (`<Name>: Template`). The
/// placeholders in double braces are filled in for every meeting.
pub const DEFAULT_TEMPLATE: &str = "\
**{{plenum}}** on {{weekday}}, {{date}} at {{time}}

| Facilitation | Minutes | Present |
|---|---|---|
|  |  |  |

## Agenda

➕ New items from the chat (!top) go here.

## Minutes

A heading (###) for each item, and below it what was discussed.

## Decisions and tasks

A line that starts with \"Decision:\" goes into the list of decisions after the plenum. \
Tasks as a checklist (type \"[]\" at the start of a line in Outline): who does what by when. \
What stays open here is in the next minutes again.

{{open_tasks}}
";

/// Where the open tasks of the last protocol go in a template; a template without it gets
/// them at its end, under [`OPEN_TASKS_HEADING`]. The German name of older templates works too.
pub const OPEN_TASKS_PLACEHOLDERS: [&str; 2] = ["{{open_tasks}}", "{{offene_aufgaben}}"];

pub const OPEN_TASKS_HEADING: &str = "## Open tasks from the last plenum";

/// The labels of decision and task lines (any case), English and German.
const DECISION_LABELS: [&str; 2] = ["decision", "beschluss"];
const TASK_LABELS: [&str; 2] = ["task", "aufgabe"];

/// The open tasks as a checklist (each with the day it comes from): ticked off they are done,
/// left open they come along to the next protocol again.
pub fn open_tasks_block(tasks: &[Carried]) -> String {
    if tasks.is_empty() {
        return String::new();
    }
    let mut block = format!("{OPEN_TASKS_HEADING}\n\n");
    for t in tasks {
        let text = t.text.replace('[', "(").replace(']', ")");
        block.push_str(&format!("- [ ] {text} (since {})\n", describe_day(t.date)));
    }
    block
}

/// Fill in a template for one meeting, with the open tasks of the last protocol. The German
/// placeholders of older templates give German dates, as their text around them is German.
pub fn render_template(
    template: &str,
    plenum: &Plenum,
    key: MeetingKey,
    tasks: &[Carried],
) -> String {
    let date = key.0.date();
    let time = key.0.format("%H:%M").to_string();
    let tasks_block = open_tasks_block(tasks);
    let mut text = template
        .replace("{{plenum}}", &plenum.name)
        .replace("{{weekday}}", &date.format("%A").to_string())
        .replace("{{date}}", &date.format("%-d %B %Y").to_string())
        .replace("{{time}}", &time)
        .replace("{{wochentag}}", weekday_de(date.weekday()))
        .replace("{{datum}}", &date.format("%d.%m.%Y").to_string())
        .replace("{{uhrzeit}}", &time);
    for iso in ["{{iso_date}}", "{{iso_datum}}"] {
        text = text.replace(iso, &date.format("%Y-%m-%d").to_string());
    }
    if let Some(placeholder) = OPEN_TASKS_PLACEHOLDERS.iter().find(|p| text.contains(**p)) {
        text = text.replace(placeholder, tasks_block.trim_end());
        // an empty placeholder leaves no blank lines behind
        while text.contains("\n\n\n") {
            text = text.replace("\n\n\n", "\n\n");
        }
    } else if !tasks_block.is_empty() {
        text = format!("{}\n\n{tasks_block}", text.trim_end());
    }
    text
}

/// The protocol's title: "Hausplenum 2026-11-01" (sorts and searches well).
pub fn title(plenum: &Plenum, key: MeetingKey) -> String {
    format!("{} {}", plenum.name, key.0.format("%Y-%m-%d"))
}

/// The heading the bot puts chat items under when a protocol has neither the marker nor an
/// agenda section. Counts as agenda for [`extract`].
pub const CHAT_ITEMS_HEADING: &str = "## New items from the chat";

/// Every chat items heading the bot knows: [`CHAT_ITEMS_HEADING`], and the German one.
pub const CHAT_ITEMS_HEADINGS: [&str; 2] = [CHAT_ITEMS_HEADING, "## Neue Punkte aus dem Chat"];

/// One agenda item as a Markdown list line, from the chat: one line, no list or heading
/// syntax of its own, at most 300 characters. Brackets become parentheses (Outline would
/// turn a `[ ]` into a checkbox, an open task at the end), and an item that starts like a
/// decision or a task is marked as an item, so `!top Decision: …` decides nothing.
pub fn top_line(top: &Top) -> String {
    let mut text: String = top
        .text
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .replace('[', "(")
        .replace(']', ")")
        .trim_start_matches(['#', '>', '*', '-', '+', '|', '_'])
        .trim()
        .chars()
        .take(300)
        .collect();
    if text.is_empty() {
        text = "…".to_owned();
    }
    if DECISION_LABELS
        .iter()
        .chain(&TASK_LABELS)
        .any(|label| after_label(&text, label).is_some())
    {
        text = format!("Item: {text}");
    }
    format!("* {} ({})", text, top.by)
}

/// The text Outline gives back may escape characters (`c\\*d`): compare without escapes.
pub fn contains_line(markdown: &str, line: &str) -> bool {
    markdown.replace('\\', "").contains(&line.replace('\\', ""))
}

/// The protocol with `line` added to the agenda: right after the last list item above the
/// marker (so it continues that list), or above the marker; without a marker at the end of
/// the agenda section; without that section in a new one at the end.
pub fn insert_top(markdown: &str, line: &str) -> String {
    let lines: Vec<&str> = markdown.lines().collect();
    let mut out: Vec<String> = lines.iter().map(|l| (*l).to_owned()).collect();
    if let Some(marker) = lines.iter().position(|l| TOP_MARKERS.contains(&l.trim())) {
        let before = lines[..marker].iter().rposition(|l| !l.trim().is_empty());
        match before {
            Some(i) if is_list_item(lines[i]) => out.insert(i + 1, line.to_owned()),
            _ => {
                out.insert(marker, String::new());
                out.insert(marker, line.to_owned());
            }
        }
        return join(out, markdown);
    }
    if let Some(heading) = lines.iter().position(|l| is_agenda_heading(l)) {
        let end = lines[heading + 1..]
            .iter()
            .position(|l| l.trim_start().starts_with('#'))
            .map(|i| heading + 1 + i)
            .unwrap_or(lines.len());
        let last = lines[heading + 1..end]
            .iter()
            .rposition(|l| !l.trim().is_empty())
            .map(|i| heading + 1 + i);
        match last {
            Some(i) if is_list_item(lines[i]) => out.insert(i + 1, line.to_owned()),
            Some(i) => {
                out.insert(i + 1, line.to_owned());
                out.insert(i + 1, String::new());
            }
            None => {
                out.insert(heading + 1, line.to_owned());
                out.insert(heading + 1, String::new());
            }
        }
        return join(out, markdown);
    }
    let mut text = markdown.trim_end().to_owned();
    text.push_str("\n\n## Agenda\n\n");
    text.push_str(line);
    text.push('\n');
    text
}

fn join(lines: Vec<String>, original: &str) -> String {
    let mut text = lines.join("\n");
    if original.ends_with('\n') {
        text.push('\n');
    }
    text
}

fn is_list_item(line: &str) -> bool {
    let t = line.trim_start();
    t.starts_with("* ") || t.starts_with("- ") || t.starts_with("+ ") || {
        let digits = t.chars().take_while(|c| c.is_ascii_digit()).count();
        digits > 0 && t[digits..].starts_with(". ")
    }
}

/// A heading of the agenda or of the chat items (English or German).
pub fn is_agenda_heading(line: &str) -> bool {
    let t = line.trim_start();
    t.starts_with('#') && {
        let title = t.trim_start_matches('#').trim().to_lowercase();
        [
            "agenda",
            "tagesordnung",
            "new items from the chat",
            "neue punkte aus dem chat",
        ]
        .iter()
        .any(|h| title.starts_with(h))
    }
}

/// What a finished protocol says.
#[derive(Debug, Default, PartialEq)]
pub struct Outcome {
    pub decisions: Vec<String>,
    /// Open checklist items (and lines starting with "Task:").
    pub tasks: Vec<String>,
}

/// Decisions are lines that start with "Decision:" (also bold, also as a list item); tasks
/// are open checklist items and lines that start with "Task:" (the German "Beschluss:" and
/// "Aufgabe:" too). Empty ones are left out, and so is the agenda: what is put on it (also
/// from the chat) is no decision yet.
pub fn extract(markdown: &str) -> Outcome {
    let mut out = Outcome::default();
    let mut agenda = false;
    for raw in markdown.lines() {
        let line = raw.trim();
        if line.starts_with('#') {
            agenda = is_agenda_heading(line);
            continue;
        }
        if agenda {
            continue;
        }
        let (checkbox, rest) = strip_list_marker(line);
        let rest = rest.trim();
        if let Some(text) = DECISION_LABELS.iter().find_map(|l| after_label(rest, l)) {
            if !text.is_empty() {
                out.decisions.push(text);
            }
            continue;
        }
        if let Some(text) = TASK_LABELS.iter().find_map(|l| after_label(rest, l)) {
            if !text.is_empty() && checkbox != Some(true) {
                out.tasks.push(text);
            }
            continue;
        }
        if checkbox == Some(false) {
            let text = clean(rest);
            if !text.is_empty() {
                out.tasks.push(text);
            }
        }
    }
    out
}

/// `(checkbox, rest)`: `Some(done)` for a checklist item, `None` otherwise; `rest` is the
/// line without its list marker.
fn strip_list_marker(line: &str) -> (Option<bool>, &str) {
    let mut rest = line;
    for marker in ["* ", "- ", "+ "] {
        if let Some(r) = rest.strip_prefix(marker) {
            rest = r;
            break;
        }
    }
    if rest.len() == line.len() {
        let digits = rest.chars().take_while(|c| c.is_ascii_digit()).count();
        if digits > 0 && rest[digits..].starts_with(". ") {
            rest = &rest[digits + 2..];
        }
    }
    for (box_, done) in [("[ ] ", false), ("[x] ", true), ("[X] ", true)] {
        if let Some(r) = rest.strip_prefix(box_) {
            return (Some(done), r);
        }
    }
    if rest == "[ ]" {
        return (Some(false), "");
    }
    (None, rest)
}

/// The text after "<label>:" at the start of `line` (any case, the label maybe in ** or _),
/// cleaned of emphasis.
fn after_label(line: &str, label: &str) -> Option<String> {
    let plain = line.trim_start_matches(['*', '_']);
    if !plain
        .get(..label.len())
        .is_some_and(|head| head.eq_ignore_ascii_case(label))
    {
        return None;
    }
    let after = &plain[label.len()..];
    let after = after.trim_start_matches(['*', '_']);
    let after = after.strip_prefix(':')?;
    let after = after.trim_start_matches(['*', '_']);
    Some(clean(after))
}

/// Without emphasis markers and Outline's escapes, trimmed.
fn clean(text: &str) -> String {
    let mut s = text.replace("**", "").replace("__", "");
    s = s.replace("\\", "");
    s.trim().trim_matches(['*', '_']).trim().to_owned()
}

/// The block for the plenum's decision list.
pub fn decisions_block(title: &str, link: &str, decisions: &[String]) -> String {
    let mut block = format!("### [{title}]({link})\n\n");
    for d in decisions {
        // Outline reads appended Markdown: a literal "[ ]" would become a checkbox.
        let d = d.replace('[', "\\[").replace(']', "\\]");
        block.push_str(&format!("* {d}\n"));
    }
    block
}

pub fn decisions_doc_text(plenum: &Plenum) -> String {
    format!(
        "The decisions from the minutes of {}, newest at the bottom. \
         The bot adds them after every plenum.\n",
        plenum.name
    )
}

pub fn rules_doc_text(plenum: &Plenum) -> String {
    format!(
        "What applies in {} right now: agreements, schedules, who does what. You keep this \
         page yourselves. When a decision changes something, it goes here; the minutes stay \
         as they were.\n",
        plenum.name
    )
}

pub fn protocols_doc_text(plenum: &Plenum) -> String {
    format!(
        "All minutes of {}: below, one per meeting. The bot creates new ones here.\n",
        plenum.name
    )
}

/// What the overview links to.
pub struct OverviewLinks<'a> {
    pub rules: &'a str,
    pub decisions: &'a str,
    pub protocols: &'a str,
    pub template: &'a str,
    /// The calendars in Nextcloud (where the dates are kept), if known.
    pub calendar: Option<&'a str>,
}

/// "1 day", "3 days"
fn days(n: u32) -> String {
    if n == 1 {
        "1 day".to_owned()
    } else {
        format!("{n} days")
    }
}

/// The overview page the bot keeps (it overwrites it whenever something changes): the next
/// dates with their agendas, how to bring in a topic, open tasks, the latest decisions, links.
pub fn overview_text(
    plenum: &Plenum,
    next: &[(MeetingKey, Option<String>)],
    tasks: &[Carried],
    decisions: &[Carried],
    links: &OverviewLinks<'_>,
) -> String {
    let esc = |s: &str| s.replace('[', "\\[").replace(']', "\\]");
    let mut t = format!(
        "_The plenum bot rewrites this page whenever something changes. What applies is in \
         the [rules]({}); your own texts belong there._\n\n## Next meetings\n\n",
        links.rules
    );
    if next.is_empty() {
        t.push_str("No meeting planned.\n");
    }
    for (key, protocol) in next {
        match protocol {
            Some(link) => t.push_str(&format!(
                "* {}: [agenda and minutes]({link})\n",
                describe_meeting(*key)
            )),
            None => t.push_str(&format!(
                "* {} (the agenda comes {} before)\n",
                describe_meeting(*key),
                days(plenum.prepare_days)
            )),
        }
    }
    let calendar = match links.calendar {
        Some(c) => format!("[“{}” in Nextcloud]({c})", plenum.name),
        None => format!("“{}” in Nextcloud", plenum.name),
    };
    t.push_str(&format!(
        "\nThe meetings are in the calendar {calendar}: add, move or cancel them there, or \
         make a series. This page, the agendas and the reminders follow it.\n"
    ));
    t.push_str(
        "\n## Bringing in a topic\n\nWrite `!top <topic>` in the chat, reply to a message \
         with `!top`, or write it into the agenda of the next meeting.\n",
    );
    t.push_str("\n## Open tasks\n\n");
    if tasks.is_empty() {
        t.push_str("None.\n");
    }
    for task in tasks {
        t.push_str(&format!(
            "* {} (since the [plenum on {}]({}))\n",
            esc(&task.text),
            describe_day(task.date),
            task.link
        ));
    }
    t.push_str("\n## Latest decisions\n\n");
    if decisions.is_empty() {
        t.push_str("None yet.\n");
    }
    for d in decisions.iter().rev() {
        t.push_str(&format!(
            "* {} ([{}]({}))\n",
            esc(&d.text),
            describe_day(d.date),
            d.link
        ));
    }
    t.push_str(&format!(
        "\n## Everything about {}\n\n* [{}]({}): what applies now\n* [{}]({}): every decision \
         with its minutes\n* [{}]({}): the archive, one per meeting\n* [{}]({}): how all \
         minutes start (you can change it)\n",
        plenum.name,
        plenum.rules_title(),
        links.rules,
        plenum.decisions_title(),
        links.decisions,
        plenum.protocols_title(),
        links.protocols,
        plenum.template_title(),
        links.template,
    ));
    t
}

/// Said once when a meeting that was announced leaves the calendar.
pub fn cancellation(plenum: &Plenum, key: MeetingKey, link: &str) -> String {
    format!(
        "❌ **{}** on {} is cancelled (no longer in the calendar). The minutes stay in the wiki: {link}",
        plenum.name,
        describe_meeting(key)
    )
}

/// The announcement a few days before (again after a move: `moved_from`).
pub fn announcement(
    plenum: &Plenum,
    key: MeetingKey,
    link: &str,
    tops: usize,
    moved_from: Option<MeetingKey>,
) -> String {
    let moved = moved_from
        .map(|old| format!(" (moved from {})", describe_meeting(old)))
        .unwrap_or_default();
    let mut text = format!(
        "📋 **{}** on {}{moved}\n\nAgenda and minutes: {link}\n\nAdd a topic: `!top <topic>`, \
         reply to a message with `!top`, or write it into the document.",
        plenum.name,
        describe_meeting(key)
    );
    match tops {
        0 => {}
        1 => text.push_str("\n\n1 item from the chat is in it already."),
        n => text.push_str(&format!("\n\n{n} items from the chat are in it already.")),
    }
    text
}

/// `started`: the bot came back during the meeting.
pub fn reminder(plenum: &Plenum, key: MeetingKey, link: &str, started: bool) -> String {
    let verb = if started { "started" } else { "starts" };
    format!(
        "⏰ **{}** {verb} at {}.\n\nMinutes: {link}",
        plenum.name,
        key.0.format("%H:%M")
    )
}

/// The summary when the protocol is closed.
pub fn summary(
    plenum: &Plenum,
    key: MeetingKey,
    link: &str,
    outcome: &Outcome,
    decisions_link: Option<&str>,
    pdf: PdfNote,
) -> String {
    let mut text = format!(
        "✅ The minutes of **{}** on {} are closed: {link}",
        plenum.name,
        describe_meeting(key)
    );
    if outcome.decisions.is_empty() {
        text.push_str("\n\nNo decisions recorded.");
    } else {
        text.push_str("\n\n**Decisions**\n");
        for d in &outcome.decisions {
            text.push_str(&format!("\n- {d}"));
        }
        if let Some(l) = decisions_link {
            text.push_str(&format!("\n\nAll decisions: {l}"));
        }
    }
    if !outcome.tasks.is_empty() {
        text.push_str("\n\n**Open tasks**\n");
        for t in &outcome.tasks {
            text.push_str(&format!("\n- {t}"));
        }
    }
    match pdf {
        PdfNote::Fine => {}
        PdfNote::Failed => text.push_str("\n\n⚠️ The PDF could not be made."),
        PdfNote::OnlyInWiki => text.push_str(
            "\n\n⚠️ The PDF did not go into the chat; it is attached to the minutes in the wiki.",
        ),
    }
    text
}

/// What the summary says about the PDF.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PdfNote {
    Fine,
    Failed,
    OnlyInWiki,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn plenum() -> Plenum {
        Plenum {
            key: "haus".into(),
            name: "Hausplenum".into(),
            room: "!r:x".try_into().unwrap(),
            collection: "Hausplenum".into(),
            calendar: "plenum-haus".into(),
            prepare_days: 3,
            remind_minutes: 30,
            duration_minutes: 120,
            close_after_hours: 12,
            organizers: HashSet::new(),
            group: None,
            access: crate::config::Access::House,
        }
    }

    fn key() -> MeetingKey {
        "2026-11-01 19:00".parse().unwrap()
    }

    fn top(text: &str) -> Top {
        Top {
            text: text.into(),
            by: "Anna".into(),
            at: chrono::Utc::now(),
        }
    }

    #[test]
    fn the_template_is_filled_in() {
        let text = render_template(DEFAULT_TEMPLATE, &plenum(), key(), &[]);
        assert!(
            text.starts_with("**Hausplenum** on Sunday, 1 November 2026 at 19:00"),
            "{text}"
        );
        assert!(text.contains(TOP_MARKER));
        assert!(!text.contains("{{"));
        assert!(!text.contains(OPEN_TASKS_HEADING));
        assert_eq!(title(&plenum(), key()), "Hausplenum 2026-11-01");
        // The template's own hints are no decision and no task.
        assert_eq!(extract(&text), Outcome::default());
    }

    #[test]
    fn an_older_german_template_still_works() {
        let old = "**{{plenum}}** am {{wochentag}}, {{datum}} um {{uhrzeit}} Uhr ({{iso_datum}})\n\n\
                   ## Tagesordnung\n\n➕ Neue Punkte kommen mit !top im Chat hier dazu.\n\n{{offene_aufgaben}}\n";
        let tasks = [carried("Ben: Putzplan")];
        let text = render_template(old, &plenum(), key(), &tasks);
        assert!(
            text.starts_with("**Hausplenum** am Sonntag, 01.11.2026 um 19:00 Uhr (2026-11-01)"),
            "{text}"
        );
        assert!(
            text.contains("- [ ] Ben: Putzplan (since 18 Oct 2026)"),
            "{text}"
        );
        assert!(!text.contains("{{"));
        assert_eq!(
            insert_top(&text, "* Kaffee (Anna)")
                .matches("* Kaffee (Anna)\n\n➕ Neue")
                .count(),
            1
        );
    }

    #[test]
    fn an_item_continues_the_list_above_the_marker() {
        // As Outline stores it after a first item.
        for marker in TOP_MARKERS {
            let doc = format!("## Agenda\n\n* Bins (Ben)\n\n{marker}\n\n## Minutes\n");
            let out = insert_top(&doc, &top_line(&top("Coffee")));
            assert_eq!(
                out,
                format!("## Agenda\n\n* Bins (Ben)\n* Coffee (Anna)\n\n{marker}\n\n## Minutes\n")
            );
        }
    }

    #[test]
    fn the_first_item_goes_above_the_marker() {
        let doc = format!("## Agenda\n\n{TOP_MARKER}\n\n## Minutes\n");
        let out = insert_top(&doc, "* Coffee (Anna)");
        assert_eq!(
            out,
            format!("## Agenda\n\n* Coffee (Anna)\n\n{TOP_MARKER}\n\n## Minutes\n")
        );
    }

    #[test]
    fn without_the_marker_the_item_ends_the_agenda_section() {
        let doc = "## Agenda\n\n1. Bins\n2. Coffee\n\n## Minutes\n\ntext\n";
        assert_eq!(
            insert_top(doc, "* Garden (Anna)"),
            "## Agenda\n\n1. Bins\n2. Coffee\n* Garden (Anna)\n\n## Minutes\n\ntext\n"
        );
        let doc = "## Tagesordnung\n\nWir reden über:\n\n## Protokoll\n";
        assert_eq!(
            insert_top(doc, "* Garten (Anna)"),
            "## Tagesordnung\n\nWir reden über:\n\n* Garten (Anna)\n\n## Protokoll\n"
        );
        let doc = "Just text.";
        assert_eq!(
            insert_top(doc, "* Garden (Anna)"),
            "Just text.\n\n## Agenda\n\n* Garden (Anna)\n"
        );
    }

    #[test]
    fn a_chat_item_cannot_bring_its_own_markdown_structure() {
        assert_eq!(
            top_line(&top("## Heading\nsecond line")),
            "* Heading second line (Anna)"
        );
        assert_eq!(top_line(&top("   ")), "* … (Anna)");
        assert_eq!(
            top_line(&top(&"x".repeat(400))).len(),
            "* ".len() + 300 + " (Anna)".len()
        );
    }

    #[test]
    fn chat_items_bring_no_checkbox_and_no_decision() {
        assert_eq!(
            top_line(&top("Dishwasher [ ] broken")),
            "* Dishwasher ( ) broken (Anna)"
        );
        assert_eq!(top_line(&top("[x] done")), "* (x) done (Anna)");
        assert_eq!(
            top_line(&top("Decision: the cash box to Bob")),
            "* Item: Decision: the cash box to Bob (Anna)"
        );
        assert_eq!(
            top_line(&top("**Beschluss:** Kasse an Bob")),
            "* Item: Beschluss:** Kasse an Bob (Anna)"
        );
        assert_eq!(
            top_line(&top("aufgabe: putzen")),
            "* Item: aufgabe: putzen (Anna)"
        );
        assert_eq!(top_line(&top("Task force")), "* Task force (Anna)");
        // An undefused line on the agenda (typed by hand) is no decision either.
        for (agenda, chat) in [
            ("## Agenda", CHAT_ITEMS_HEADINGS[0]),
            ("## Tagesordnung", CHAT_ITEMS_HEADINGS[1]),
        ] {
            let doc = format!(
                "{agenda}\n\n* Decision: cash box to Bob (Mallory)\n\n## Minutes\n\nDecision: real\n\n{chat}\n\n- [ ] no task\n"
            );
            let o = extract(&doc);
            assert_eq!(o.decisions, vec!["real"]);
            assert!(o.tasks.is_empty(), "{:?}", o.tasks);
        }
    }

    #[test]
    fn escaped_lines_are_found_again() {
        assert!(contains_line("* c\\*d (Anna)", "* c*d (Anna)"));
        assert!(!contains_line("* cd (Anna)", "* c*d (Anna)"));
    }

    #[test]
    fn decisions_and_tasks_are_read_back() {
        let doc = "\
## Minutes

### Bins

Decision: The bins stay in the yard from now on.
**Decision:** We buy a compost bin.
* **Beschluss**: Ben fragt die Hausverwaltung.
Decision:
Decisions were taken quickly.

## Tasks

- [ ] Anna: order the compost bin by Friday
- [x] Ben: done already
- [ ]
* Task: paint a sign
* Aufgabe: Schild malen
";
        let o = extract(doc);
        assert_eq!(
            o.decisions,
            vec![
                "The bins stay in the yard from now on.",
                "We buy a compost bin.",
                "Ben fragt die Hausverwaltung."
            ]
        );
        assert_eq!(
            o.tasks,
            vec![
                "Anna: order the compost bin by Friday",
                "paint a sign",
                "Schild malen"
            ]
        );
    }

    #[test]
    fn the_summary_lists_what_was_decided() {
        let o = Outcome {
            decisions: vec!["Compost".into()],
            tasks: vec!["Anna orders".into()],
        };
        let s = summary(
            &plenum(),
            key(),
            "https://w/doc/x",
            &o,
            Some("https://w/doc/b"),
            PdfNote::Failed,
        );
        assert!(s.contains("- Compost"));
        assert!(s.contains("- Anna orders"));
        assert!(s.contains("All decisions: https://w/doc/b"));
        assert!(s.contains("PDF"));
        let none = summary(
            &plenum(),
            key(),
            "l",
            &Outcome::default(),
            None,
            PdfNote::Fine,
        );
        assert!(none.contains("No decisions"));
    }

    fn carried(text: &str) -> Carried {
        Carried {
            text: text.into(),
            date: chrono::NaiveDate::from_ymd_opt(2026, 10, 18).unwrap(),
            link: "https://wiki.test/doc/old".into(),
        }
    }

    #[test]
    fn open_tasks_come_along_as_a_checklist_and_are_read_back() {
        let tasks = [carried("Anna: dishwasher [repair]")];
        let text = render_template(DEFAULT_TEMPLATE, &plenum(), key(), &tasks);
        assert!(text.contains(OPEN_TASKS_HEADING));
        assert!(text.contains("- [ ] Anna: dishwasher (repair) (since 18 Oct 2026)"));
        assert!(!text.contains("{{"));
        // left open: a task again at the end of this meeting
        assert_eq!(
            extract(&text).tasks,
            vec!["Anna: dishwasher (repair) (since 18 Oct 2026)".to_owned()]
        );
        // ticked off: done
        let done = text.replace("- [ ] Anna", "- [x] Anna");
        assert!(extract(&done).tasks.is_empty());
        // a template of its own without the placeholder: at the end
        let own = render_template("**{{plenum}}**\n\n## Agenda\n", &plenum(), key(), &tasks);
        assert!(own.trim_end().ends_with("(since 18 Oct 2026)"));
        assert_eq!(
            render_template("x {{open_tasks}}", &plenum(), key(), &[]).trim(),
            "x"
        );
    }

    #[test]
    fn the_overview_lists_dates_tasks_decisions_and_links() {
        let next = [
            (key(), Some("https://wiki.test/doc/p".to_owned())),
            ("2026-11-15 19:00".parse().unwrap(), None),
        ];
        let links = OverviewLinks {
            rules: "https://wiki.test/doc/rules",
            decisions: "https://wiki.test/doc/dec",
            protocols: "https://wiki.test/doc/protocols",
            template: "https://wiki.test/doc/tpl",
            calendar: Some("https://cloud.test/apps/calendar"),
        };
        let t = overview_text(
            &plenum(),
            &next,
            &[carried("Ben: cleaning rota")],
            &[carried("Decision [one]")],
            &links,
        );
        assert!(t.contains("[agenda and minutes](https://wiki.test/doc/p)"));
        assert!(
            t.contains("15 November 2026") && t.contains("3 days before"),
            "{t}"
        );
        assert!(
            t.contains("[“Hausplenum” in Nextcloud](https://cloud.test/apps/calendar)"),
            "{t}"
        );
        assert!(t.contains(
            "* Ben: cleaning rota (since the [plenum on 18 Oct 2026](https://wiki.test/doc/old))"
        ));
        assert!(t.contains("Decision \\[one\\]"));
        assert!(t.contains("[Hausplenum: Rules](https://wiki.test/doc/rules)"));
        // the overview's own lines are no task and no decision
        assert_eq!(extract(&t), Outcome::default());
    }
}
