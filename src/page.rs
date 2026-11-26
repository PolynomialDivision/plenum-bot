//! A meeting's page read as topics (docs/PLENUM-PAGES.md §4.3): what was decided, discussed or
//! not reached, the topics carried onto the next page, the result box at the top and the
//! topic a `!top` adds. Pure text functions on the Markdown Outline gives back.
//!
//! Outline gives back its own spelling (checked against 1.12.0, `a_page_in_a_real_outline`):
//! emphasis as `*`/`**` (underline `__`), list items as `* `, most escapes dropped (`_`, `[`
//! without `](`, `#` inside a line), a Shift-Enter break as `\n` inside the line.

use crate::{
    protocol::{after_label, clean, for_matrix, strip_list_marker, DECISION_LABELS},
    schedule::describe_day,
    state::Top,
};
use chrono::NaiveDate;

/// Where the result box goes (the top of the default template). Plain text: Outline keeps it.
pub const RESULT_MARKER: &str =
    "➕ The bot writes the result here after the plenum (please leave this line).";

/// Where topics from the chat and carried topics go (the end of "Topics").
pub const TOPIC_MARKER: &str =
    "➕ Topics from the chat are added above this line (please leave it).";

/// The bold title the result box starts with; a page that has it has its box.
pub const BOX_TITLE: &str = "Result of the plenum";

/// The heading topics go under on a page that has none (an edited template).
pub const TOPICS_HEADING: &str = "## Topics";

const TOPICS: [&str; 2] = ["topics", "themen"];
/// Headings that end the topics (exactly these titles, any level).
const ENDS: [&str; 4] = [
    "tasks",
    "aufgaben",
    "open tasks",
    "open tasks from the last plenum",
];
const RESULT_LABELS: [&str; 2] = ["result", "ergebnis"];
const CARRIED_LINE: &str = "Carried over from the";

/// The page's lines: a Shift-Enter break (Outline writes it as `\n` inside the line) splits a
/// line, and a line that is only `\` (an empty paragraph) is empty. An escaped backslash
/// (`\\`) stays.
pub fn lines(markdown: &str) -> Vec<String> {
    let mut out = Vec::new();
    for raw in markdown.lines() {
        let mut line = String::new();
        let mut chars = raw.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '\\' {
                match chars.peek() {
                    Some('\\') => {
                        line.push_str("\\\\");
                        chars.next();
                    }
                    Some('n') => {
                        chars.next();
                        out.push(std::mem::take(&mut line));
                    }
                    _ => line.push(c),
                }
            } else {
                line.push(c);
            }
        }
        if line.trim() == "\\" {
            line.clear();
        }
        out.push(line);
    }
    out
}

/// `(level, title)` of a heading line, the title lower case without emphasis.
fn heading(line: &str) -> Option<(usize, String)> {
    let t = line.trim_start();
    let level = t.chars().take_while(|c| *c == '#').count();
    if level == 0 || level > 6 || !t[level..].starts_with(' ') {
        return None;
    }
    let title = clean(&t[level..])
        .trim_end_matches(':')
        .trim()
        .to_lowercase();
    Some((level, title))
}

/// A heading that ends the topics: "Tasks" and the like, at the level of "Topics" or above (a
/// topic of that name further down is a topic).
fn is_ending(level: usize, title: &str, topics_level: usize) -> bool {
    level <= topics_level && ENDS.contains(&title)
}

/// The kinds of notice Outline has (`:::info` …).
const NOTICES: [&str; 4] = [":::info", ":::success", ":::warning", ":::tip"];

/// Whether `lines[i]` opens a notice that is closed further down.
fn opens_notice<S: AsRef<str>>(lines: &[S], i: usize) -> bool {
    NOTICES.contains(&lines[i].as_ref().trim())
        && lines[i + 1..].iter().any(|l| l.as_ref().trim() == ":::")
}

/// The level of the page's topics: the first heading's under "Topics", else one below it.
pub fn topics_level(markdown: &str) -> usize {
    let ls: Vec<&str> = markdown.lines().collect();
    let Some(start) = topics_heading(&ls) else {
        return 3;
    };
    let own = heading(ls[start]).map_or(2, |(lv, _)| lv);
    ls[start + 1..]
        .iter()
        .map_while(|l| match heading(l) {
            Some((lv, t)) if is_ending(lv, &t, own) => None,
            h => Some(h),
        })
        .flatten()
        .map(|(lv, _)| lv)
        .next()
        .unwrap_or((own + 1).min(6))
}

/// A block of the bot (its heading `### `) at the page's topics level.
pub fn at_level(block: &str, level: usize) -> String {
    match block.strip_prefix("### ") {
        Some(rest) if level != 3 => format!("{} {rest}", "#".repeat(level.clamp(1, 6))),
        _ => block.to_owned(),
    }
}

/// The index of the "Topics" heading among `lines`.
fn topics_heading<S: AsRef<str>>(lines: &[S]) -> Option<usize> {
    lines
        .iter()
        .position(|l| heading(l.as_ref()).is_some_and(|(_, t)| TOPICS.contains(&t.as_str())))
}

/// The bot's marker lines (also the one of the template before 2026-11-07).
pub fn is_marker(line: &str) -> bool {
    let t = line.trim();
    t == RESULT_MARKER || t == TOPIC_MARKER || t == crate::protocol::TOP_MARKER
}

/// How a topic was read at close.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Reading {
    /// A decision in the topic.
    Decided,
    /// Something after "Result:", no decision.
    Discussed,
    /// "Result:" with nothing after it.
    NotReached,
    /// No "Result:" line and no decision: carried, so nothing is lost.
    NoResult,
    /// Two "Result:" lines (reordered or pasted): told, not carried.
    Check,
}

impl Reading {
    pub fn carried(self) -> bool {
        matches!(self, Reading::NotReached | Reading::NoResult)
    }
}

/// One topic of a page.
#[derive(Clone, Debug, PartialEq)]
pub struct Topic {
    /// The heading's text as Outline wrote it.
    pub heading: String,
    pub reading: Reading,
    /// The lines before its result (all of them without a result line), notices and the
    /// bot's markers left out, the "Carried over" line too.
    pub preparation: Vec<String>,
    /// Its decisions, cleaned.
    pub decisions: Vec<String>,
}

impl Topic {
    /// The title as text: without emphasis (Outline's `*`, `**`, `__`) or escapes; a single
    /// `_` is a literal one.
    pub fn title(&self) -> String {
        plain(&self.heading)
    }
}

/// Outline's Markdown of a line as text: escapes resolved, emphasis markers dropped.
fn plain(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                if let Some(next) = chars.next() {
                    out.push(next);
                }
            }
            '*' => {}
            '_' if chars.peek() == Some(&'_') => {
                chars.next();
            }
            _ => out.push(c),
        }
    }
    out.trim().to_owned()
}

/// Whether the page is written in topics (it has a "Topics" heading): `!top` adds a topic
/// there, a line to the agenda elsewhere (pages of the old template).
pub fn has_topics(markdown: &str) -> bool {
    let ls: Vec<&str> = markdown.lines().collect();
    topics_heading(&ls).is_some()
}

/// The page's topics (§4.3): the headings after "Topics" up to "Tasks" (or the end). The
/// first one's level is the topics' level: a heading at that level or above starts a topic, a
/// deeper one belongs to the topic above it.
pub fn topics(markdown: &str) -> Vec<Topic> {
    let ls = lines(markdown);
    let Some(start) = topics_heading(&ls) else {
        return Vec::new();
    };
    let own = heading(&ls[start]).map_or(2, |(lv, _)| lv);
    // the area, without notices (the help) and markers
    let mut area: Vec<&str> = Vec::new();
    let mut notice = false;
    for (i, l) in ls.iter().enumerate().skip(start + 1) {
        let t = l.trim();
        if notice {
            notice = t != ":::";
            continue;
        }
        if opens_notice(&ls, i) {
            notice = true;
            continue;
        }
        if heading(l).is_some_and(|(lv, title)| is_ending(lv, &title, own)) {
            break;
        }
        if is_marker(l) {
            continue;
        }
        area.push(l);
    }
    let Some(level) = area.iter().find_map(|l| heading(l)).map(|(lv, _)| lv) else {
        return Vec::new();
    };
    let starts = |l: &str| heading(l).is_some_and(|(lv, _)| lv <= level);
    let mut out = Vec::new();
    let mut i = 0;
    while i < area.len() {
        if !starts(area[i]) {
            i += 1;
            continue;
        }
        let head = area[i]
            .trim_start()
            .trim_start_matches('#')
            .trim()
            .to_owned();
        let end = (i + 1..area.len())
            .find(|&j| starts(area[j]))
            .unwrap_or(area.len());
        out.push(read(head, &area[i + 1..end]));
        i = end;
    }
    out
}

/// The text after a "Result:" label at the start of the line.
fn result_label(line: &str) -> Option<String> {
    let (_, rest) = strip_list_marker(line.trim());
    RESULT_LABELS
        .iter()
        .find_map(|label| after_label(rest.trim(), label))
}

/// The decision a line holds: one starting with "Decision:", also right after "Result:".
pub(crate) fn decision_in(line: &str) -> Option<String> {
    let (_, rest) = strip_list_marker(line.trim());
    let rest = rest.trim();
    let after_result = RESULT_LABELS
        .iter()
        .find_map(|label| after_label(rest, label));
    let text = after_result.as_deref().unwrap_or(rest);
    DECISION_LABELS
        .iter()
        .find_map(|label| after_label(text, label))
        .filter(|t| !t.is_empty())
}

fn read(heading: String, body: &[&str]) -> Topic {
    let body: Vec<&str> = body
        .iter()
        .copied()
        .filter(|l| !clean(l).replace(['*', '_'], "").starts_with(CARRIED_LINE))
        .collect();
    let results: Vec<usize> = (0..body.len())
        .filter(|&i| result_label(body[i]).is_some())
        .collect();
    let decisions: Vec<String> = body.iter().filter_map(|l| decision_in(l)).collect();
    let reading = match results.as_slice() {
        [_, _, ..] => Reading::Check,
        _ if !decisions.is_empty() => Reading::Decided,
        [] => Reading::NoResult,
        [r] => {
            let same_line = result_label(body[*r]).unwrap_or_default();
            let after = body[r + 1..].iter().any(|l| !clean(l).is_empty());
            if same_line.is_empty() && !after {
                Reading::NotReached
            } else {
                Reading::Discussed
            }
        }
    };
    let until = results.first().copied().unwrap_or(body.len());
    let mut preparation: Vec<String> = body[..until].iter().map(|l| (*l).to_owned()).collect();
    while preparation.first().is_some_and(|l| l.trim().is_empty()) {
        preparation.remove(0);
    }
    while preparation.last().is_some_and(|l| l.trim().is_empty()) {
        preparation.pop();
    }
    Topic {
        heading,
        reading,
        preparation,
        decisions,
    }
}

/// Mentions as plain names (a copy by the bot must not notify anyone again).
fn plain_mentions(text: &str) -> String {
    for_matrix(text, |_| None).0
}

/// Outline's Markdown sent back: a single `_` it returns is a literal one (its emphasis is
/// `*`, its underline `__`), so it is escaped, or it would become emphasis.
fn resend(line: &str) -> String {
    let chars: Vec<char> = line.chars().collect();
    let mut out = String::with_capacity(line.len());
    for (i, c) in chars.iter().enumerate() {
        let lone = *c == '_'
            && chars.get(i.wrapping_sub(1)) != Some(&'_')
            && chars.get(i + 1) != Some(&'_')
            && (i == 0 || chars[i - 1] != '\\');
        if lone {
            out.push('\\');
        }
        out.push(*c);
    }
    out
}

/// A topic carried onto the next page: its preparation with a line saying from where, open
/// checkboxes as plain points (they would be tasks twice), mentions as names, and an empty
/// result. The heading is written the way `!top` writes one, so the next page shows it as
/// it was.
pub fn carried_block(topic: &Topic, from: NaiveDate, link: &str) -> String {
    let why = match topic.reading {
        Reading::NoResult => " It had no \"Result:\" line; if it is done, delete it here",
        _ => "",
    };
    let mut block = format!(
        "### {}\n\n*{CARRIED_LINE} [plenum on {}]({link}).{why}*\n\n",
        literal(&plain_mentions(&topic.title())),
        describe_day(from)
    );
    for l in &topic.preparation {
        let indent = &l[..l.len() - l.trim_start().len()];
        let line = match strip_list_marker(l.trim()) {
            // nobody decided it here (a cancelled meeting's page): a proposal again
            _ if decision_in(l).is_some() => {
                format!("{indent}Proposal: {}", decision_in(l).unwrap_or_default())
            }
            (Some(false), rest) => format!("{indent}- {rest}"),
            _ => l.clone(),
        };
        block.push_str(&resend(&plain_mentions(&line)));
        block.push('\n');
    }
    if !topic.preparation.is_empty() {
        block.push('\n');
    }
    block.push_str("**Result:**");
    block
}

/// The heading line a block starts with.
pub fn block_heading(block: &str) -> &str {
    block.lines().next().unwrap_or_default()
}

/// A heading's title for comparing: no emphasis, escapes, case or spaces that Outline may
/// spell differently.
fn comparable(heading_line: &str) -> String {
    clean(heading_line.trim_start().trim_start_matches('#'))
        .replace(['*', '_', '\\'], "")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// Whether the page has a topic with the title of `heading_line` (a carried topic that is
/// there already, however Outline spells it).
pub fn has_heading(markdown: &str, heading_line: &str) -> bool {
    let wanted = comparable(heading_line);
    topics(markdown)
        .iter()
        .any(|t| comparable(&t.heading) == wanted)
}

/// Text from the chat as Markdown that stays text: no syntax of its own, escapes for what
/// Outline would read as formatting.
fn literal(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    if text.starts_with(':') {
        out.push('\\');
    }
    // ➕ as + : the bot's markers are lines of its own, never someone's text
    for c in text.replace('➕', "+").chars() {
        match c {
            '\\' | '*' | '_' | '`' | '~' | '[' | ']' | '<' | '>' | '#' | '|' => {
                out.push('\\');
                out.push(c);
            }
            _ => out.push(c),
        }
    }
    out
}

/// A topic from the chat (`!top`): the first line is its title, the rest what it is about.
/// A line that starts like a decision becomes a proposal, one like a result or a task an item:
/// `!top` decides nothing.
pub fn topic_block(top: &Top) -> String {
    let mut parts = top.text.lines().map(str::trim).filter(|l| !l.is_empty());
    let title: String = parts
        .next()
        .unwrap_or("…")
        .trim_start_matches(['#', '>', '*', '-', '+', '|', '_'])
        .trim()
        .chars()
        .take(200)
        .collect();
    let title = if title.is_empty() {
        "…".to_owned()
    } else {
        title
    };
    let by: String = top
        .by
        .chars()
        .filter(|c| !c.is_control())
        .take(60)
        .collect();
    let mut block = format!(
        "### {}\n\n**From:** {} (from the chat)\n\n",
        literal(&title),
        literal(&by)
    );
    let mut length = 0;
    for line in parts {
        length += line.len();
        if length > 1500 {
            block.push_str("…\n\n");
            break;
        }
        let line = match decision_in(line) {
            Some(proposal) if result_label(line).is_none() => format!("Proposal: {proposal}"),
            _ if result_label(line).is_some()
                || decision_in(line).is_some()
                || after_label(line, "task").is_some() =>
            {
                format!("Item: {line}")
            }
            _ => line.to_owned(),
        };
        block.push_str(&literal(&line));
        block.push_str("\n\n");
    }
    block.push_str("**Result:**");
    block
}

/// The page, with `block` added at the topics' marker (above it), else at the end of its
/// topics, else under a "Topics" heading at its end (for a page that is being made).
pub fn insert_block(markdown: &str, block: &str) -> String {
    let ls: Vec<&str> = markdown.lines().collect();
    let block = &at_level(block, topics_level(markdown));
    let at = ls
        .iter()
        .position(|l| l.trim() == TOPIC_MARKER)
        .or_else(|| {
            let start = topics_heading(&ls)?;
            let own = heading(ls[start]).map_or(2, |(lv, _)| lv);
            Some(
                (start + 1..ls.len())
                    .find(|&j| heading(ls[j]).is_some_and(|(lv, t)| is_ending(lv, &t, own)))
                    .unwrap_or(ls.len()),
            )
        });
    match at {
        Some(i) => {
            let mut out: Vec<String> = ls[..i].iter().map(|l| (*l).to_owned()).collect();
            while out.last().is_some_and(|l| l.trim().is_empty()) {
                out.pop();
            }
            out.push(String::new());
            out.push(block.to_owned());
            out.push(String::new());
            out.extend(ls[i..].iter().map(|l| (*l).to_owned()));
            let mut text = out.join("\n");
            if markdown.ends_with('\n') {
                text.push('\n');
            }
            text
        }
        None => format!("{}\n\n{TOPICS_HEADING}\n\n{block}\n", markdown.trim_end()),
    }
}

/// The line of the heading that ends a page's topics ("Tasks" and the like), if it has one:
/// a block that goes in above it is the last topic.
pub fn end_of_topics(markdown: &str) -> Option<&str> {
    let ls: Vec<&str> = markdown.lines().collect();
    let start = topics_heading(&ls)?;
    let own = heading(ls[start]).map_or(2, |(lv, _)| lv);
    ls[start + 1..]
        .iter()
        .find(|l| heading(l).is_some_and(|(lv, t)| is_ending(lv, &t, own)))
        .copied()
}

/// Whether the page has its result box.
pub fn has_box(markdown: &str) -> bool {
    markdown.lines().any(|l| clean(l).starts_with(BOX_TITLE))
}

/// The page without its result box (it repeats the decisions) and without the template's
/// help (`:::info`): neither is read for decisions or tasks.
pub fn without_box(markdown: &str) -> String {
    let mut out = Vec::new();
    let mut in_notice = false;
    let mut notice: Vec<&str> = Vec::new();
    let ls: Vec<&str> = markdown.lines().collect();
    for (i, l) in ls.iter().copied().enumerate() {
        let t = l.trim();
        if in_notice {
            notice.push(l);
            if t == ":::" {
                let help = notice[0].trim() == ":::info";
                if !help && !notice.iter().any(|n| clean(n).starts_with(BOX_TITLE)) {
                    out.append(&mut notice);
                }
                notice.clear();
                in_notice = false;
            }
            continue;
        }
        if opens_notice(&ls, i) {
            in_notice = true;
            notice.push(l);
            continue;
        }
        out.push(l);
    }
    out.append(&mut notice);
    out.join("\n")
}

/// The result box (§4.4): what was decided (by topic) and discussed, what moves on, the tasks.
/// `decisions`: all of the page's, also those outside a topic.
pub fn result_box(decisions: &[String], topics: &[Topic], tasks: &[String]) -> String {
    let name = |t: &Topic| literal(&plain_mentions(&t.title()));
    let names = |r: Reading| -> Vec<String> {
        topics
            .iter()
            .filter(|t| t.reading == r)
            .map(&name)
            .collect()
    };
    let mut items: Vec<String> = Vec::new();
    let mut shown: Vec<&String> = Vec::new();
    for t in topics.iter().filter(|t| t.reading == Reading::Decided) {
        for d in &t.decisions {
            items.push(format!("✅ {}: {}", name(t), literal(&plain_mentions(d))));
            shown.push(d);
        }
    }
    for d in decisions.iter().filter(|d| !shown.contains(d)) {
        items.push(format!("✅ Decided: {}", literal(&plain_mentions(d))));
    }
    let discussed = names(Reading::Discussed);
    if !discussed.is_empty() {
        items.push(format!("💬 Discussed: {}", discussed.join(", ")));
    }
    let mut moved: Vec<String> = names(Reading::NotReached)
        .into_iter()
        .map(|n| format!("{n} (not reached)"))
        .collect();
    moved.extend(
        names(Reading::NoResult)
            .into_iter()
            .map(|n| format!("{n} (no result written)")),
    );
    if !moved.is_empty() {
        items.push(format!("⏭️ On the next page: {}", moved.join(", ")));
    }
    let check = names(Reading::Check);
    if !check.is_empty() {
        items.push(format!(
            "❓ Two \"Result:\" lines, so not carried over (copy it to the next page if it is still open): {}",
            check.join(", ")
        ));
    }
    if !tasks.is_empty() {
        let tasks: Vec<String> = tasks.iter().map(|t| literal(&plain_mentions(t))).collect();
        items.push(format!("📌 Tasks: {}", tasks.join("; ")));
    }
    if items.is_empty() {
        items.push("Nothing was written down.".to_owned());
    }
    let mut text = format!(
        ":::success\n**{BOX_TITLE}** (filled in by the bot when the minutes were closed)\n\n"
    );
    for i in items {
        text.push_str(&format!("- {i}\n"));
    }
    text.push_str(":::");
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page() -> String {
        format!(
            "\
**House plenum** · Sunday, 16 November 2026, 19:00

{RESULT_MARKER}

## Topics

:::info
- New topic: type ### and a space, then the title. Last line: Result:
:::

### Washing machine

**From:** Anna

It is broken. Proposal: a new one.

- Ben: repair it?

**Result:** We talked.\\nDecision: we buy a new one

### Bike cellar

Too full.

**Result:**

### Garden

Notes only, no result line.

### Party

**Result:** fine

* Result: again

{TOPIC_MARKER}

## Tasks

- [ ] Anna: order it
"
        )
    }

    #[test]
    fn shift_enter_and_empty_paragraphs_split_into_lines() {
        assert_eq!(lines("a\\nb\n\\\nc\\\\nd"), ["a", "b", "", "c\\\\nd"]);
    }

    #[test]
    fn a_page_is_read_as_topics() {
        let t = topics(&page());
        let got: Vec<(String, Reading)> = t.iter().map(|t| (t.title(), t.reading)).collect();
        assert_eq!(
            got,
            [
                ("Washing machine".to_owned(), Reading::Decided),
                ("Bike cellar".to_owned(), Reading::NotReached),
                ("Garden".to_owned(), Reading::NoResult),
                ("Party".to_owned(), Reading::Check),
            ]
        );
        assert_eq!(t[0].decisions, ["we buy a new one"]);
        assert_eq!(t[1].preparation, ["Too full."]);
        // the marker after the last topic is no result, the help is no topic
        assert!(t
            .iter()
            .all(|t| !t.preparation.iter().any(|l| l.contains('➕'))));
    }

    #[test]
    fn the_first_headings_level_is_the_topics_level() {
        let page = "## Topics\n\n## Bins\n\n### Background\n\nOld.\n\n**Result:**\n\n## Aufgaben\n\n## Garden\n";
        let t = topics(page);
        assert_eq!(t.len(), 1);
        assert_eq!(t[0].reading, Reading::NotReached);
        assert!(t[0].preparation.contains(&"### Background".to_owned()));
        // a bigger heading among the topics is a topic too, and ends nothing
        let page =
            "## Topics\n\n### A\n\nResult:\n\n# B\n\nResult: ok\n\n### C\n\nResult:\n\n## Tasks\n";
        let got: Vec<(String, Reading)> = topics(page)
            .iter()
            .map(|t| (t.title(), t.reading))
            .collect();
        assert_eq!(
            got,
            [
                ("A".to_owned(), Reading::NotReached),
                ("B".to_owned(), Reading::Discussed),
                ("C".to_owned(), Reading::NotReached),
            ]
        );
        // a topic named like a task list is a topic
        assert_eq!(
            topics("## Topics\n\n### Tasks for the garden\n\nResult:\n").len(),
            1
        );
    }

    #[test]
    fn german_labels_and_results_on_the_same_line() {
        let page = "## Themen\n\n### Müll\n\n* **Ergebnis:** vertagt auf Januar\n\n### Hof\n\nErgebnis:\n\nBeschluss: Bank kaufen\n\n### Tor\n\nResult: Decision: it gets a lock\n";
        let t = topics(page);
        assert_eq!(t[0].reading, Reading::Discussed);
        assert_eq!(t[1].reading, Reading::Decided);
        assert_eq!(t[2].reading, Reading::Decided);
        assert_eq!(t[2].decisions, ["it gets a lock"]);
        assert_eq!(
            crate::protocol::extract(page).decisions,
            ["Bank kaufen", "it gets a lock"]
        );
    }

    #[test]
    fn a_page_without_topics_has_none() {
        assert!(topics("## Agenda\n\n* Bins\n\n## Minutes\n\n### Bins\n\nResult:\n").is_empty());
        assert!(!has_topics(
            "## Agenda\n\n➕ New items from the chat (!top) go here.\n"
        ));
        assert!(has_topics(&page()));
    }

    #[test]
    fn a_carried_topic_keeps_its_preparation_and_drops_tasks_and_mentions() {
        let topic = Topic {
            // as Outline gives it back: the escape of `_` dropped
            heading: "Bike @[Cara](mention://1/user/2) _now_".into(),
            reading: Reading::NoResult,
            preparation: vec![
                "Too full, the snake_case sign.".into(),
                String::new(),
                "* [ ] count the bikes".into(),
                "*Carried over from the* [*plenum on 1 Nov*](x)*.*".into(),
            ],
            decisions: Vec::new(),
        };
        let b = carried_block(
            &topic,
            NaiveDate::from_ymd_opt(2026, 11, 16).unwrap(),
            "https://w/doc/a",
        );
        assert!(
            b.starts_with("### Bike @Cara \\_now\\_\n\n*Carried over from the [plenum on "),
            "{b}"
        );
        assert!(b.contains("no \"Result:\" line"), "{b}");
        assert!(b.contains("snake\\_case"), "{b}");
        assert!(b.contains("- count the bikes"), "{b}");
        assert!(!b.contains("[ ]"), "{b}");
        assert!(b.ends_with("**Result:**"));
        // read again on the next page: not reached, the old line gone, the same title
        let page = format!("## Topics\n\n{b}\n");
        let again = topics(&page);
        assert_eq!(again[0].reading, Reading::NotReached);
        assert!(!again[0]
            .preparation
            .iter()
            .any(|l| l.contains("Carried over")));
        // found however Outline spells the heading back
        assert!(has_heading(
            "## Topics\n\n### Bike @Cara *now*\n",
            block_heading(&b)
        ));
        assert!(has_heading(
            "## Topics\n\n### Bike @Cara _now_\n",
            block_heading(&b)
        ));
        assert!(!has_heading("## Topics\n\n### Bike\n", block_heading(&b)));
    }

    #[test]
    fn a_topic_from_the_chat_is_text_and_decides_nothing() {
        let top = Top {
            text: format!("# Bins *now*\nDecision: yellow\nResult: done\n{TOPIC_MARKER}"),
            by: "Anna [x](y)".into(),
            at: Default::default(),
        };
        let b = topic_block(&top);
        assert!(
            b.starts_with("### Bins \\*now\\*\n\n**From:** Anna \\[x\\](y) (from the chat)"),
            "{b}"
        );
        assert!(b.contains("Proposal: yellow"), "{b}");
        assert!(b.contains("Item: Result: done"), "{b}");
        let page = format!("## Topics\n\n{b}\n\n{TOPIC_MARKER}\n");
        let t = topics(&page);
        assert_eq!(t.len(), 1);
        assert_eq!(t[0].reading, Reading::NotReached, "{page}");
        assert!(crate::protocol::extract(&page).decisions.is_empty());
        // the marker in a topic's text is not a marker
        assert_eq!(page.lines().filter(|l| l.trim() == TOPIC_MARKER).count(), 1);
    }

    #[test]
    fn blocks_go_above_the_marker_or_at_the_end_of_the_topics() {
        let b = "### New\n\n**Result:**";
        let with = insert_block(&page(), b);
        let i = with.find("### New").unwrap();
        assert!(i < with.find(TOPIC_MARKER).unwrap() && i > with.find("### Party").unwrap());
        let without = insert_block("## Topics\n\n### A\n\n## Tasks\n", b);
        assert_eq!(
            without,
            "## Topics\n\n### A\n\n### New\n\n**Result:**\n\n## Tasks\n"
        );
        assert_eq!(end_of_topics(&without), Some("## Tasks"));
        let old = insert_block("## Agenda\n\n* x\n", b);
        assert!(old.ends_with(&format!("{TOPICS_HEADING}\n\n{b}\n")));
        assert_eq!(topics(&old).len(), 1);
    }

    #[test]
    fn the_box_says_what_came_of_it_and_is_read_once() {
        let text = page();
        let t = topics(&text);
        let all = crate::protocol::extract(&text).decisions;
        let mut decisions = all.clone();
        decisions.push("outside any topic".into());
        let b = result_box(&decisions, &t, &["Anna: order it".into()]);
        assert!(b.contains("- ✅ Washing machine: we buy a new one"), "{b}");
        assert!(b.contains("- ✅ Decided: outside any topic"), "{b}");
        assert!(
            b.contains("Bike cellar (not reached), Garden (no result written)"),
            "{b}"
        );
        assert!(b.contains("not carried over (copy it"), "{b}");
        let page = text.replace(RESULT_MARKER, &format!("\n\n{b}\n\n"));
        assert!(has_box(&page));
        assert!(!has_box(&text));
        let plain = without_box(&page);
        assert!(!plain.contains("Washing machine: we buy"));
        assert!(plain.contains("### Washing machine"));
        assert_eq!(crate::protocol::extract(&page).decisions, all);
        assert_eq!(topics(&page), t);
    }

    #[test]
    fn the_bots_blocks_go_in_at_the_pages_topics_level() {
        let page = "## Topics\n\n## Bins\n\nResult:\n\n## Tasks\n";
        assert_eq!(topics_level(page), 2);
        let b = "### New\n\n**Result:**";
        let with = insert_block(page, b);
        assert!(with.contains("## New\n"), "{with}");
        let got: Vec<String> = topics(&with).iter().map(|t| t.title()).collect();
        assert_eq!(got, ["Bins", "New"]);
        assert_eq!(topics_level("## Topics\n\n## Tasks\n"), 3);
        assert_eq!(topics_level("# Topics\n"), 2);
    }

    #[test]
    fn a_topic_named_tasks_is_a_topic_and_tasks_end_only_at_the_top() {
        let page =
            "## Topics\n\n### Tasks\n\nResult:\n\n### Bins\n\nResult:\n\n## Tasks\n\n### Bins\n";
        let got: Vec<String> = topics(page).iter().map(|t| t.title()).collect();
        assert_eq!(got, ["Tasks", "Bins"]);
        // a heading outside the topics is not a topic there already
        assert!(!has_heading(
            "## Topics\n\n### A\n\n#### Background\n",
            "### Background"
        ));
        assert!(has_heading(page, "### Bins"));
    }

    #[test]
    fn chat_text_starts_no_notice_and_a_lone_notice_line_hides_nothing() {
        let top = Top {
            text: "Bins\n:::warning look\nmore".into(),
            by: "Anna".into(),
            at: Default::default(),
        };
        let page = format!(
            "## Topics\n\n{}\n\n### Next\n\nResult:\n",
            topic_block(&top)
        );
        let t = topics(&page);
        assert_eq!(t.len(), 2, "{page}");
        assert_eq!(t[0].reading, Reading::NotReached);
        let lone = "## Topics\n\n### A\n\n:::warning\n\nResult:\n\n### B\n\nResult:\n";
        assert_eq!(topics(lone).len(), 2);
    }

    #[test]
    fn a_carried_decision_nobody_took_is_a_proposal_again() {
        let topic = Topic {
            heading: "Bins".into(),
            reading: Reading::Decided,
            preparation: vec!["Decision: yellow ones".into()],
            decisions: vec!["yellow ones".into()],
        };
        let b = carried_block(&topic, NaiveDate::from_ymd_opt(2026, 11, 1).unwrap(), "x");
        assert!(b.contains("Proposal: yellow ones"), "{b}");
        assert!(crate::protocol::extract(&b).decisions.is_empty());
    }

    #[test]
    fn the_help_is_never_read_for_decisions_or_tasks() {
        let page = "## Topics\n\n:::info\nDecision: an example\n- [ ] an example task\n:::\n";
        assert_eq!(
            crate::protocol::extract(page),
            crate::protocol::Outcome::default()
        );
        // other notices people write stay in
        let note = "## Topics\n\n:::tip\nDecision: kept\n:::\n";
        assert_eq!(crate::protocol::extract(note).decisions, ["kept"]);
    }
}
