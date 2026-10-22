//! The protocol as Markdown: the template, agenda items from the chat, and what is read back
//! at the end (decisions and tasks). Pure text functions; Outline stores Markdown and gives
//! it back in its own spelling (`*` for bullets, `- [ ]` for checkboxes).

use crate::{
    config::Plenum,
    schedule::{describe_meeting, weekday_de, MeetingKey},
    state::{Carried, Top},
};
use chrono::Datelike;

/// The line agenda items from the chat are put above. It is plain text on purpose: Outline
/// keeps it exactly as it is, while formatting would come back in another spelling.
pub const TOP_MARKER: &str = "➕ Neue Punkte kommen mit !top im Chat hier dazu.";

/// The template a plenum starts with when its collection has none (`Vorlage <Name>`). The
/// placeholders in double braces are filled in for every meeting.
pub const DEFAULT_TEMPLATE: &str = "\
**{{plenum}}** am {{wochentag}}, {{datum}} um {{uhrzeit}} Uhr

| Moderation | Protokoll | Anwesend |
|---|---|---|
|  |  |  |

## Tagesordnung

➕ Neue Punkte kommen mit !top im Chat hier dazu.

## Protokoll

Zu jedem Punkt eine Überschrift (###) und darunter, was besprochen wurde.

## Beschlüsse und Aufgaben

Eine Zeile, die mit „Beschluss:“ beginnt, kommt nach dem Plenum in die Beschlussliste. \
Aufgaben als Checkliste (in Outline „[]“ am Zeilenanfang tippen): wer macht was bis wann. \
Was hier offen bleibt, steht im nächsten Protokoll wieder da.

{{offene_aufgaben}}
";

/// Where the open tasks of the last protocol go in a template; a template without it gets
/// them at its end, under [`OPEN_TASKS_HEADING`].
pub const OPEN_TASKS_PLACEHOLDER: &str = "{{offene_aufgaben}}";

pub const OPEN_TASKS_HEADING: &str = "## Offene Aufgaben vom letzten Plenum";

/// The open tasks as a checklist (each with the day it comes from): ticked off they are done,
/// left open they come along to the next protocol again.
pub fn open_tasks_block(tasks: &[Carried]) -> String {
    if tasks.is_empty() {
        return String::new();
    }
    let mut block = format!("{OPEN_TASKS_HEADING}\n\n");
    for t in tasks {
        let text = t.text.replace('[', "(").replace(']', ")");
        block.push_str(&format!(
            "- [ ] {text} (seit {})\n",
            t.date.format("%d.%m.%Y")
        ));
    }
    block
}

/// Fill in a template for one meeting, with the open tasks of the last protocol.
pub fn render_template(
    template: &str,
    plenum: &Plenum,
    key: MeetingKey,
    tasks: &[Carried],
) -> String {
    let date = key.0.date();
    let tasks_block = open_tasks_block(tasks);
    let mut text = template
        .replace("{{plenum}}", &plenum.name)
        .replace("{{wochentag}}", weekday_de(date.weekday()))
        .replace("{{datum}}", &date.format("%d.%m.%Y").to_string())
        .replace("{{iso_datum}}", &date.format("%Y-%m-%d").to_string())
        .replace("{{uhrzeit}}", &key.0.format("%H:%M").to_string());
    if text.contains(OPEN_TASKS_PLACEHOLDER) {
        text = text.replace(OPEN_TASKS_PLACEHOLDER, tasks_block.trim_end());
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
pub const CHAT_ITEMS_HEADING: &str = "## Neue Punkte aus dem Chat";

/// One agenda item as a Markdown list line, from the chat: one line, no list or heading
/// syntax of its own, at most 300 characters. Brackets become parentheses (Outline would
/// turn a `[ ]` into a checkbox, an open task at the end), and an item that starts like a
/// decision or a task is marked as an item, so `!top Beschluss: …` decides nothing.
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
    let lower = text.to_lowercase();
    if lower.starts_with("beschluss") || lower.starts_with("aufgabe") {
        text = format!("TOP {text}");
    }
    format!("* {} ({})", text, top.by)
}

/// The text Outline gives back may escape characters (`c\\*d`): compare without escapes.
pub fn contains_line(markdown: &str, line: &str) -> bool {
    markdown.replace('\\', "").contains(&line.replace('\\', ""))
}

/// The protocol with `line` added to the agenda: right after the last list item above the
/// marker (so it continues that list), or above the marker; without a marker at the end of
/// the "Tagesordnung" section; without that section in a new one at the end.
pub fn insert_top(markdown: &str, line: &str) -> String {
    let lines: Vec<&str> = markdown.lines().collect();
    let mut out: Vec<String> = lines.iter().map(|l| (*l).to_owned()).collect();
    if let Some(marker) = lines.iter().position(|l| l.trim() == TOP_MARKER) {
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
    text.push_str("\n\n## Tagesordnung\n\n");
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

fn is_agenda_heading(line: &str) -> bool {
    let t = line.trim_start();
    t.starts_with('#') && {
        let title = t.trim_start_matches('#').trim().to_lowercase();
        title.starts_with("tagesordnung") || title.starts_with("neue punkte aus dem chat")
    }
}

/// What a finished protocol says.
#[derive(Debug, Default, PartialEq)]
pub struct Outcome {
    pub decisions: Vec<String>,
    /// Open checklist items (and lines starting with "Aufgabe:").
    pub tasks: Vec<String>,
}

/// Decisions are lines that start with "Beschluss:" (also bold, also as a list item); tasks
/// are open checklist items and lines that start with "Aufgabe:". Empty ones are left out,
/// and so is the agenda: what is put on it (also from the chat) is no decision yet.
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
        if let Some(text) = after_label(rest, "beschluss") {
            if !text.is_empty() {
                out.decisions.push(text);
            }
            continue;
        }
        if let Some(text) = after_label(rest, "aufgabe") {
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
        "Die Beschlüsse aus den Protokollen von {}, neueste unten. \
         Der Bot trägt sie nach jedem Plenum ein.\n",
        plenum.name
    )
}

pub fn rules_doc_text(plenum: &Plenum) -> String {
    format!(
        "Was beim {} gerade gilt: Absprachen, Pläne, Zuständigkeiten. Diese Seite pflegt ihr \
         selbst. Wenn ein Beschluss etwas ändert, kommt es hierher; die Protokolle bleiben, \
         wie sie waren.\n",
        plenum.name
    )
}

pub fn protocols_doc_text(plenum: &Plenum) -> String {
    format!(
        "Alle Protokolle vom {}: darunter, eins pro Termin. Neue legt der Bot hier an.\n",
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
        "_Diese Seite schreibt der Plenum-Bot neu, sobald sich etwas ändert. Was gilt, steht \
         in den [Regeln]({}); eigene Texte gehören dorthin._\n\n## Nächste Termine\n\n",
        links.rules
    );
    if next.is_empty() {
        t.push_str("Kein Termin geplant.\n");
    }
    for (key, protocol) in next {
        match protocol {
            Some(link) => t.push_str(&format!(
                "* {}: [Tagesordnung und Protokoll]({link})\n",
                describe_meeting(*key)
            )),
            None => t.push_str(&format!(
                "* {} (die Tagesordnung kommt {} Tag(e) vorher)\n",
                describe_meeting(*key),
                plenum.prepare_days
            )),
        }
    }
    let calendar = match links.calendar {
        Some(c) => format!("[„{}“ in Nextcloud]({c})", plenum.name),
        None => format!("„{}“ in Nextcloud", plenum.name),
    };
    t.push_str(&format!(
        "\nDie Termine stehen im Kalender {calendar}: dort eintragen, verschieben, absagen oder \
         als Serie anlegen. Diese Seite, die Tagesordnungen und die Erinnerungen folgen ihm.\n"
    ));
    t.push_str(
        "\n## Themen einbringen\n\nIm Chat `!top <Thema>` schreiben, auf eine Nachricht mit \
         `!top` antworten, oder direkt in die Tagesordnung des nächsten Termins.\n",
    );
    t.push_str("\n## Offene Aufgaben\n\n");
    if tasks.is_empty() {
        t.push_str("Keine.\n");
    }
    for task in tasks {
        t.push_str(&format!(
            "* {} (seit dem [Plenum am {}]({}))\n",
            esc(&task.text),
            task.date.format("%d.%m.%Y"),
            task.link
        ));
    }
    t.push_str("\n## Letzte Beschlüsse\n\n");
    if decisions.is_empty() {
        t.push_str("Noch keine.\n");
    }
    for d in decisions.iter().rev() {
        t.push_str(&format!(
            "* {} ([{}]({}))\n",
            esc(&d.text),
            d.date.format("%d.%m.%Y"),
            d.link
        ));
    }
    t.push_str(&format!(
        "\n## Alles zum {}\n\n* [{}]({}): was gerade gilt\n* [{}]({}): jeder Beschluss mit \
         seinem Protokoll\n* [{}]({}): das Archiv, ein Protokoll pro Termin\n* [{}]({}): so \
         beginnt jedes Protokoll (änderbar)\n",
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
        "❌ **{}** am {} ist abgesagt (nicht mehr im Kalender). Das Protokoll bleibt im Wiki: {link}",
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
        .map(|old| format!(" (verschoben vom {})", describe_meeting(old)))
        .unwrap_or_default();
    let mut text = format!(
        "📋 **{}** am {}{moved}\n\nTagesordnung und Protokoll: {link}\n\nThemen dazu: `!top <Thema>`, \
         auf eine Nachricht mit `!top` antworten, oder direkt ins Dokument schreiben.",
        plenum.name,
        describe_meeting(key)
    );
    if tops > 0 {
        text.push_str(&format!(
            "\n\n{tops} Punkt(e) aus dem Chat stehen schon drin."
        ));
    }
    text
}

/// `started`: the bot came back during the meeting.
pub fn reminder(plenum: &Plenum, key: MeetingKey, link: &str, started: bool) -> String {
    let (before, after) = if started {
        ("hat um", " Uhr begonnen")
    } else {
        ("beginnt um", " Uhr")
    };
    format!(
        "⏰ **{}** {before} {}{after}.\n\nProtokoll: {link}",
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
        "✅ Das Protokoll vom **{}** am {} ist abgeschlossen: {link}",
        plenum.name,
        describe_meeting(key)
    );
    if outcome.decisions.is_empty() {
        text.push_str("\n\nKeine Beschlüsse eingetragen.");
    } else {
        text.push_str("\n\n**Beschlüsse**\n");
        for d in &outcome.decisions {
            text.push_str(&format!("\n- {d}"));
        }
        if let Some(l) = decisions_link {
            text.push_str(&format!("\n\nAlle Beschlüsse: {l}"));
        }
    }
    if !outcome.tasks.is_empty() {
        text.push_str("\n\n**Offene Aufgaben**\n");
        for t in &outcome.tasks {
            text.push_str(&format!("\n- {t}"));
        }
    }
    match pdf {
        PdfNote::Fine => {}
        PdfNote::Failed => text.push_str("\n\n⚠️ Das PDF ließ sich nicht erstellen."),
        PdfNote::OnlyInWiki => {
            text.push_str("\n\n⚠️ Das PDF ging nicht in den Chat; es hängt im Wiki am Protokoll.")
        }
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
        assert!(text.starts_with("**Hausplenum** am Sonntag, 01.11.2026 um 19:00 Uhr"));
        assert!(text.contains(TOP_MARKER));
        assert!(!text.contains("{{"));
        assert!(!text.contains(OPEN_TASKS_HEADING));
        assert_eq!(title(&plenum(), key()), "Hausplenum 2026-11-01");
        // The template's own hints are no decision and no task.
        assert_eq!(extract(&text), Outcome::default());
    }

    #[test]
    fn an_item_continues_the_list_above_the_marker() {
        // As Outline stores it after a first item.
        let doc = format!("## Tagesordnung\n\n* Müll (Ben)\n\n{TOP_MARKER}\n\n## Protokoll\n");
        let out = insert_top(&doc, &top_line(&top("Kaffee")));
        assert_eq!(
            out,
            format!("## Tagesordnung\n\n* Müll (Ben)\n* Kaffee (Anna)\n\n{TOP_MARKER}\n\n## Protokoll\n")
        );
    }

    #[test]
    fn the_first_item_goes_above_the_marker() {
        let doc = format!("## Tagesordnung\n\n{TOP_MARKER}\n\n## Protokoll\n");
        let out = insert_top(&doc, "* Kaffee (Anna)");
        assert_eq!(
            out,
            format!("## Tagesordnung\n\n* Kaffee (Anna)\n\n{TOP_MARKER}\n\n## Protokoll\n")
        );
    }

    #[test]
    fn without_the_marker_the_item_ends_the_agenda_section() {
        let doc = "## Tagesordnung\n\n1. Müll\n2. Kaffee\n\n## Protokoll\n\ntext\n";
        assert_eq!(
            insert_top(doc, "* Garten (Anna)"),
            "## Tagesordnung\n\n1. Müll\n2. Kaffee\n* Garten (Anna)\n\n## Protokoll\n\ntext\n"
        );
        let doc = "## Tagesordnung\n\nWir reden über:\n\n## Protokoll\n";
        assert_eq!(
            insert_top(doc, "* Garten (Anna)"),
            "## Tagesordnung\n\nWir reden über:\n\n* Garten (Anna)\n\n## Protokoll\n"
        );
        let doc = "Nur Text.";
        assert_eq!(
            insert_top(doc, "* Garten (Anna)"),
            "Nur Text.\n\n## Tagesordnung\n\n* Garten (Anna)\n"
        );
    }

    #[test]
    fn a_chat_item_cannot_bring_its_own_markdown_structure() {
        assert_eq!(
            top_line(&top("## Überschrift\nzweite Zeile")),
            "* Überschrift zweite Zeile (Anna)"
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
            top_line(&top("Spülmaschine [ ] kaputt")),
            "* Spülmaschine ( ) kaputt (Anna)"
        );
        assert_eq!(top_line(&top("[x] erledigt")), "* (x) erledigt (Anna)");
        assert_eq!(
            top_line(&top("Beschluss: Kasse an Bob")),
            "* TOP Beschluss: Kasse an Bob (Anna)"
        );
        assert_eq!(
            top_line(&top("aufgabe: putzen")),
            "* TOP aufgabe: putzen (Anna)"
        );
        // An undefused line on the agenda (typed by hand) is no decision either.
        let doc = "## Tagesordnung\n\n* Beschluss: Kasse an Bob (Mallory)\n\n## Protokoll\n\nBeschluss: echt\n\n## Neue Punkte aus dem Chat\n\n- [ ] kein Task\n";
        let o = extract(doc);
        assert_eq!(o.decisions, vec!["echt"]);
        assert!(o.tasks.is_empty(), "{:?}", o.tasks);
    }

    #[test]
    fn escaped_lines_are_found_again() {
        assert!(contains_line("* c\\*d (Anna)", "* c*d (Anna)"));
        assert!(!contains_line("* cd (Anna)", "* c*d (Anna)"));
    }

    #[test]
    fn decisions_and_tasks_are_read_back() {
        let doc = "\
## Protokoll

### Müll

Beschluss: Die Tonnen stehen ab jetzt im Hof.
**Beschluss:** Wir kaufen einen Kompost.
* **Beschluss**: Ben fragt die Hausverwaltung.
Beschluss:
Beschlussfähig waren wir.

## Aufgaben

- [ ] Anna: Kompost bestellen bis Freitag
- [x] Ben: schon erledigt
- [ ]
* Aufgabe: Schild malen
";
        let o = extract(doc);
        assert_eq!(
            o.decisions,
            vec![
                "Die Tonnen stehen ab jetzt im Hof.",
                "Wir kaufen einen Kompost.",
                "Ben fragt die Hausverwaltung."
            ]
        );
        assert_eq!(
            o.tasks,
            vec!["Anna: Kompost bestellen bis Freitag", "Schild malen"]
        );
    }

    #[test]
    fn the_summary_lists_what_was_decided() {
        let o = Outcome {
            decisions: vec!["Kompost".into()],
            tasks: vec!["Anna bestellt".into()],
        };
        let s = summary(
            &plenum(),
            key(),
            "https://w/doc/x",
            &o,
            Some("https://w/doc/b"),
            PdfNote::Failed,
        );
        assert!(s.contains("- Kompost"));
        assert!(s.contains("- Anna bestellt"));
        assert!(s.contains("Alle Beschlüsse: https://w/doc/b"));
        assert!(s.contains("PDF"));
        let none = summary(
            &plenum(),
            key(),
            "l",
            &Outcome::default(),
            None,
            PdfNote::Fine,
        );
        assert!(none.contains("Keine Beschlüsse"));
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
        let tasks = [carried("Anna: Spülmaschine [reparieren]")];
        let text = render_template(DEFAULT_TEMPLATE, &plenum(), key(), &tasks);
        assert!(text.contains(OPEN_TASKS_HEADING));
        assert!(text.contains("- [ ] Anna: Spülmaschine (reparieren) (seit 18.10.2026)"));
        assert!(!text.contains("{{"));
        // left open: a task again at the end of this meeting
        assert_eq!(
            extract(&text).tasks,
            vec!["Anna: Spülmaschine (reparieren) (seit 18.10.2026)".to_owned()]
        );
        // ticked off: done
        let done = text.replace("- [ ] Anna", "- [x] Anna");
        assert!(extract(&done).tasks.is_empty());
        // a template of its own without the placeholder: at the end
        let own = render_template(
            "**{{plenum}}**\n\n## Tagesordnung\n",
            &plenum(),
            key(),
            &tasks,
        );
        assert!(own.trim_end().ends_with("(seit 18.10.2026)"));
        assert_eq!(
            render_template("x {{offene_aufgaben}}", &plenum(), key(), &[]).trim(),
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
            &[carried("Ben: Putzplan")],
            &[carried("Beschluss [eins]")],
            &links,
        );
        assert!(t.contains("[Tagesordnung und Protokoll](https://wiki.test/doc/p)"));
        assert!(t.contains("15.11.2026") && t.contains("3 Tag(e) vorher"));
        assert!(
            t.contains("[„Hausplenum“ in Nextcloud](https://cloud.test/apps/calendar)"),
            "{t}"
        );
        assert!(t.contains(
            "* Ben: Putzplan (seit dem [Plenum am 18.10.2026](https://wiki.test/doc/old))"
        ));
        assert!(t.contains("Beschluss \\[eins\\]"));
        assert!(t.contains("[Regeln Hausplenum](https://wiki.test/doc/rules)"));
        // the overview's own lines are no task and no decision
        assert_eq!(extract(&t), Outcome::default());
    }
}
