//! The protocol as Markdown: the template, agenda items from the chat, and what is read back
//! at the end (decisions and tasks). Pure text functions; Outline stores Markdown and gives
//! it back in its own spelling (`*` for bullets, `- [ ]` for checkboxes).

use crate::{
    config::Plenum,
    schedule::{describe_meeting, weekday_de, MeetingKey},
    state::Top,
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
Aufgaben als Checkliste (in Outline „[]“ am Zeilenanfang tippen): wer macht was bis wann.
";

/// Fill in a template for one meeting.
pub fn render_template(template: &str, plenum: &Plenum, key: MeetingKey) -> String {
    let date = key.0.date();
    template
        .replace("{{plenum}}", &plenum.name)
        .replace("{{wochentag}}", weekday_de(date.weekday()))
        .replace("{{datum}}", &date.format("%d.%m.%Y").to_string())
        .replace("{{iso_datum}}", &date.format("%Y-%m-%d").to_string())
        .replace("{{uhrzeit}}", &key.0.format("%H:%M").to_string())
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

/// The announcement a few days before.
pub fn announcement(plenum: &Plenum, key: MeetingKey, link: &str, tops: usize) -> String {
    let mut text = format!(
        "📋 **{}** am {}\n\nProtokoll und Tagesordnung: {link}\n\nPunkte für die Tagesordnung: `!top <Thema>`",
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
            recurrence: None,
            prepare_days: 3,
            remind_minutes: 30,
            duration_minutes: 120,
            close_after_hours: 12,
            organizers: HashSet::new(),
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
        let text = render_template(DEFAULT_TEMPLATE, &plenum(), key());
        assert!(text.starts_with("**Hausplenum** am Sonntag, 01.11.2026 um 19:00 Uhr"));
        assert!(text.contains(TOP_MARKER));
        assert!(!text.contains("{{"));
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
}
