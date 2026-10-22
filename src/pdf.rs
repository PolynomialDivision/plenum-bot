//! The finished protocol as a PDF, from its Markdown: headings, paragraphs with bold and
//! italic, lists with checkboxes, tables, code. Outline's own syntax for notices (`:::info`)
//! and empty paragraphs (`\`) is dropped; emoji are left out.
//!
//! Fonts: a protocol in Western European characters (Windows-1252, German included) uses
//! the PDF reader's own Helvetica, which is not embedded: some 20 KB. Anything else
//! (Cyrillic, other scripts) needs DejaVu Sans embedded whole, about 1.5 MB, and is printed
//! without italics (only two fonts are embedded).

use std::path::Path;

use anyhow::{Context, Result};
use genpdf::{elements, fonts, style, Alignment, Element};
use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};

use crate::protocol::TOP_MARKER;

#[derive(Clone, Debug, Default, PartialEq)]
struct Span {
    text: String,
    bold: bool,
    italic: bool,
}

/// A table row: its cells, each a run of spans.
type Row = Vec<Vec<Span>>;

#[derive(Debug, PartialEq)]
enum Block {
    Heading(u8, Vec<Span>),
    Para(Vec<Span>),
    List { ordered: bool, items: Vec<Item> },
    Table(Vec<Row>),
    Code(String),
    Rule,
}

#[derive(Debug, Default, PartialEq)]
struct Item {
    check: Option<bool>,
    blocks: Vec<Block>,
}

enum Frame {
    Root(Vec<Block>),
    List { ordered: bool, items: Vec<Item> },
    Item(Item),
    Quote(Vec<Block>),
}

impl Frame {
    fn push(&mut self, block: Block) {
        match self {
            Frame::Root(b) | Frame::Quote(b) => b.push(block),
            Frame::Item(i) => i.blocks.push(block),
            Frame::List { items, .. } => items.push(Item {
                check: None,
                blocks: vec![block],
            }),
        }
    }
}

/// Outline's Markdown without what only Outline understands.
fn outline_cleanup(markdown: &str) -> String {
    markdown
        .lines()
        .filter(|l| {
            let t = l.trim();
            t != "\\" && t != TOP_MARKER && !(t.starts_with(":::") && !t.contains(' '))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Without emoji and pictographs: DejaVu Sans has no glyphs for them. The checkbox
/// symbols and check marks stay.
fn printable(text: &str) -> String {
    text.chars()
        .filter(|c| {
            let u = *c as u32;
            let keep = matches!(*c, '☐' | '☑' | '☒' | '✓' | '✔' | '✗' | '•');
            keep || !(matches!(u, 0x1F000..=0x1FFFF | 0x2600..=0x27BF | 0xFE00..=0xFE0F | 0x200D))
        })
        .collect()
}

/// Whether the PDF reader's own fonts can show `c` (Windows-1252).
fn in_cp1252(c: char) -> bool {
    let u = c as u32;
    matches!(u, 0x20..=0x7E | 0xA0..=0xFF) || "€‚ƒ„…†‡ˆ‰Š‹ŒŽ‘’“”•–—˜™š›œžŸ".contains(c)
}

/// How a PDF is drawn: with the reader's own fonts or with DejaVu embedded.
#[derive(Clone, Copy)]
struct Look {
    builtin: bool,
}

impl Look {
    /// The cheap fonts when every character of the PDF has a glyph in them.
    fn for_text(text: &str) -> Self {
        let builtin = Look { builtin: true }
            .text(&printable(text))
            .chars()
            .all(|c| c == '\n' || c == '\r' || c == '\t' || in_cp1252(c));
        Look { builtin }
    }

    /// Text as this look prints it: checkboxes as `[ ]`/`[x]` in the reader's fonts.
    fn text(self, text: &str) -> String {
        if !self.builtin {
            return text.to_owned();
        }
        text.replace('☐', "[ ]")
            .replace(['☑', '☒'], "[x]")
            .replace(['✓', '✔', '✗'], "x")
    }

    fn checkbox(self, done: bool) -> String {
        self.text(if done { "☑ " } else { "☐ " })
    }
}

fn parse(markdown: &str) -> Vec<Block> {
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_TABLES);
    opts.insert(Options::ENABLE_TASKLISTS);
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    let cleaned = outline_cleanup(markdown);
    let mut stack = vec![Frame::Root(Vec::new())];
    let mut spans: Vec<Span> = Vec::new();
    let (mut bold, mut italic) = (0u32, 0u32);
    let mut heading: Option<u8> = None;
    let mut code: Option<String> = None;
    // (rows so far, the row being read)
    let mut table: Option<(Vec<Row>, Row)> = None;

    fn flush(stack: &mut [Frame], spans: &mut Vec<Span>) {
        if spans.iter().any(|s| !s.text.trim().is_empty()) {
            stack
                .last_mut()
                .expect("root")
                .push(Block::Para(std::mem::take(spans)));
        }
        spans.clear();
    }

    for event in Parser::new_ext(&cleaned, opts) {
        match event {
            Event::Start(Tag::Heading { level, .. }) => {
                flush(&mut stack, &mut spans);
                heading = Some(level as u8);
            }
            Event::End(TagEnd::Heading(_)) => {
                let level = heading.take().unwrap_or(3);
                stack
                    .last_mut()
                    .expect("root")
                    .push(Block::Heading(level, std::mem::take(&mut spans)));
            }
            Event::Start(Tag::Paragraph) => flush(&mut stack, &mut spans),
            Event::End(TagEnd::Paragraph) => {
                if table.is_none() {
                    flush(&mut stack, &mut spans);
                }
            }
            Event::Start(Tag::List(start)) => {
                flush(&mut stack, &mut spans);
                stack.push(Frame::List {
                    ordered: start.is_some(),
                    items: Vec::new(),
                });
            }
            Event::End(TagEnd::List(_)) => {
                if let Some(Frame::List { ordered, items }) = stack.pop() {
                    stack
                        .last_mut()
                        .expect("root")
                        .push(Block::List { ordered, items });
                }
            }
            Event::Start(Tag::Item) => stack.push(Frame::Item(Item::default())),
            Event::End(TagEnd::Item) => {
                flush(&mut stack, &mut spans);
                if let Some(Frame::Item(item)) = stack.pop() {
                    if let Some(Frame::List { items, .. }) = stack.last_mut() {
                        items.push(item);
                    }
                }
            }
            Event::TaskListMarker(done) => {
                if let Some(Frame::Item(item)) = stack.last_mut() {
                    item.check = Some(done);
                }
            }
            Event::Start(Tag::BlockQuote(_)) => {
                flush(&mut stack, &mut spans);
                stack.push(Frame::Quote(Vec::new()));
            }
            Event::End(TagEnd::BlockQuote(_)) => {
                flush(&mut stack, &mut spans);
                if let Some(Frame::Quote(blocks)) = stack.pop() {
                    for b in blocks {
                        stack.last_mut().expect("root").push(b);
                    }
                }
            }
            Event::Start(Tag::Table(_)) => {
                flush(&mut stack, &mut spans);
                table = Some((Vec::new(), Vec::new()));
            }
            Event::End(TagEnd::Table) => {
                if let Some((rows, _)) = table.take() {
                    stack.last_mut().expect("root").push(Block::Table(rows));
                }
            }
            Event::Start(Tag::TableHead | Tag::TableRow) => {
                if let Some((_, row)) = table.as_mut() {
                    row.clear();
                }
            }
            Event::End(TagEnd::TableHead | TagEnd::TableRow) => {
                if let Some((rows, row)) = table.as_mut() {
                    rows.push(std::mem::take(row));
                }
            }
            Event::Start(Tag::TableCell) => spans.clear(),
            Event::End(TagEnd::TableCell) => {
                if let Some((_, row)) = table.as_mut() {
                    row.push(std::mem::take(&mut spans));
                }
            }
            Event::Start(Tag::CodeBlock(_)) => {
                flush(&mut stack, &mut spans);
                code = Some(String::new());
            }
            Event::End(TagEnd::CodeBlock) => {
                if let Some(c) = code.take() {
                    stack.last_mut().expect("root").push(Block::Code(c));
                }
            }
            Event::Start(Tag::Strong) => bold += 1,
            Event::End(TagEnd::Strong) => bold = bold.saturating_sub(1),
            Event::Start(Tag::Emphasis) => italic += 1,
            Event::End(TagEnd::Emphasis) => italic = italic.saturating_sub(1),
            Event::Text(t) | Event::Code(t) => {
                if let Some(c) = code.as_mut() {
                    c.push_str(&t);
                } else {
                    spans.push(Span {
                        text: printable(&t),
                        bold: bold > 0,
                        italic: italic > 0,
                    });
                }
            }
            Event::SoftBreak => spans.push(Span {
                text: " ".into(),
                ..Default::default()
            }),
            Event::HardBreak => {
                if table.is_some() {
                    spans.push(Span {
                        text: " ".into(),
                        ..Default::default()
                    });
                } else {
                    flush(&mut stack, &mut spans);
                }
            }
            Event::Rule => {
                flush(&mut stack, &mut spans);
                stack.last_mut().expect("root").push(Block::Rule);
            }
            _ => {}
        }
    }
    flush(&mut stack, &mut spans);
    // Unclosed frames (broken Markdown) are folded into their parents.
    while stack.len() > 1 {
        let frame = stack.pop().expect("len > 1");
        let blocks = match frame {
            Frame::Root(b) | Frame::Quote(b) => b,
            Frame::Item(i) => i.blocks,
            Frame::List { ordered, items } => vec![Block::List { ordered, items }],
        };
        for b in blocks {
            stack.last_mut().expect("root").push(b);
        }
    }
    match stack.pop() {
        Some(Frame::Root(b)) => b,
        _ => Vec::new(),
    }
}

/// A boxed element as an element (genpdf's containers take `impl Element`).
struct Boxed(Box<dyn Element>);

impl Element for Boxed {
    fn render(
        &mut self,
        context: &genpdf::Context,
        area: genpdf::render::Area<'_>,
        style: style::Style,
    ) -> Result<genpdf::RenderResult, genpdf::error::Error> {
        self.0.render(context, area, style)
    }
}

fn paragraph(spans: &[Span], base: style::Style, look: Look) -> elements::Paragraph {
    let mut p = elements::Paragraph::default();
    for s in spans {
        let mut st = base;
        if s.bold {
            st = st.bold();
        }
        // Embedded, there is no italic font (see the module docs).
        if s.italic && look.builtin {
            st = st.italic();
        }
        p.push_styled(look.text(&s.text), st);
    }
    p
}

fn element(block: &Block, look: Look) -> Box<dyn Element> {
    match block {
        Block::Heading(level, spans) => {
            let size = match level {
                1 => 16,
                2 => 14,
                _ => 12,
            };
            Box::new(
                paragraph(spans, style::Style::new().bold().with_font_size(size), look)
                    .padded(genpdf::Margins::trbl(3, 0, 1, 0)),
            )
        }
        Block::Para(spans) => Box::new(
            paragraph(spans, style::Style::new(), look).padded(genpdf::Margins::trbl(0, 0, 1.5, 0)),
        ),
        Block::Code(text) => {
            let mut layout = elements::LinearLayout::vertical();
            for line in text.lines() {
                layout.push(
                    elements::Paragraph::new(look.text(&printable(line)))
                        .styled(style::Style::new().with_font_size(9)),
                );
            }
            Box::new(layout.padded(genpdf::Margins::trbl(0, 0, 1.5, 4)))
        }
        Block::Rule => Box::new(elements::Break::new(1)),
        Block::List { ordered, items } => {
            let build = |items: &[Item]| -> Vec<Box<dyn Element>> {
                items.iter().map(|i| item_element(i, look)).collect()
            };
            if *ordered {
                let mut list = elements::OrderedList::new();
                for e in build(items) {
                    list.push(Boxed(e));
                }
                Box::new(list.padded(genpdf::Margins::trbl(0, 0, 1.5, 0)))
            } else {
                let mut list = elements::UnorderedList::new();
                for e in build(items) {
                    list.push(Boxed(e));
                }
                Box::new(list.padded(genpdf::Margins::trbl(0, 0, 1.5, 0)))
            }
        }
        Block::Table(rows) => {
            let cols = rows.iter().map(Vec::len).max().unwrap_or(1).max(1);
            let mut table = elements::TableLayout::new(vec![1; cols]);
            table.set_cell_decorator(elements::FrameCellDecorator::new(true, true, false));
            for (i, row) in rows.iter().enumerate() {
                let base = if i == 0 {
                    style::Style::new().bold()
                } else {
                    style::Style::new()
                };
                let mut r = table.row();
                for c in 0..cols {
                    let spans = row.get(c).cloned().unwrap_or_default();
                    r.push_element(paragraph(&spans, base, look).padded(1));
                }
                // A row with a wrong cell count cannot happen: every row is padded to `cols`.
                let _ = r.push();
            }
            Box::new(table.padded(genpdf::Margins::trbl(0, 0, 2, 0)))
        }
    }
}

fn item_element(item: &Item, look: Look) -> Box<dyn Element> {
    let mut layout = elements::LinearLayout::vertical();
    let mut first = true;
    for b in &item.blocks {
        match (first, item.check, b) {
            (true, Some(done), Block::Para(spans)) => {
                let mut with_box = vec![Span {
                    text: look.checkbox(done),
                    ..Default::default()
                }];
                with_box.extend(spans.iter().cloned());
                layout.push(paragraph(&with_box, style::Style::new(), look));
            }
            (_, _, Block::Para(spans)) => layout.push(paragraph(spans, style::Style::new(), look)),
            _ => layout.push(Boxed(element(b, look))),
        }
        first = false;
    }
    if item.blocks.is_empty() {
        let mark = item
            .check
            .map(|done| look.checkbox(done))
            .unwrap_or_default();
        layout.push(elements::Paragraph::new(mark.trim_end().to_owned()));
    }
    Box::new(layout)
}

fn fonts(dir: &Path, look: Look) -> Result<fonts::FontFamily<fonts::FontData>> {
    let read = |name: &str| -> Result<Vec<u8>> {
        let path = dir.join(name);
        std::fs::read(&path).with_context(|| format!("font {}", path.display()))
    };
    // DejaVu's metrics are used either way; with a builtin the PDF only names Helvetica.
    use printpdf::BuiltinFont as B;
    let font = |bytes: &Vec<u8>, builtin: Option<B>| -> Result<fonts::FontData> {
        fonts::FontData::new(bytes.clone(), builtin).map_err(|e| anyhow::anyhow!("font: {e}"))
    };
    let regular = read("DejaVuSans.ttf")?;
    let bold = read("DejaVuSans-Bold.ttf")?;
    let (r, b) = if look.builtin {
        (Some(B::Helvetica), Some(B::HelveticaBold))
    } else {
        (None, None)
    };
    Ok(fonts::FontFamily {
        regular: font(&regular, r)?,
        bold: font(&bold, b)?,
        // Never used when embedding (italic spans are printed upright), so never embedded.
        italic: font(&regular, Some(B::HelveticaOblique))?,
        bold_italic: font(&bold, Some(B::HelveticaBoldOblique))?,
    })
}

fn document(title: &str, subtitle: &str, font_dir: &Path, look: Look) -> Result<genpdf::Document> {
    let mut doc = genpdf::Document::new(fonts(font_dir, look)?);
    doc.set_title(title);
    // Without the ICC colour profile and XMP metadata printpdf adds by default (600 KB).
    doc.set_minimal_conformance();
    doc.set_font_size(10);
    doc.set_line_spacing(1.25);
    let mut deco = genpdf::SimplePageDecorator::new();
    deco.set_margins(18);
    deco.set_header(|page| {
        elements::Paragraph::new(format!("Page {page}"))
            .aligned(Alignment::Right)
            .styled(style::Style::new().with_font_size(8))
    });
    doc.set_page_decorator(deco);
    doc.push(
        elements::Paragraph::new(look.text(&printable(title)))
            .styled(style::Style::new().bold().with_font_size(18)),
    );
    doc.push(
        elements::Paragraph::new(look.text(&printable(subtitle)))
            .styled(style::Style::new().with_font_size(9))
            .padded(genpdf::Margins::trbl(0, 0, 4, 0)),
    );
    Ok(doc)
}

/// The protocol as PDF bytes. If the layout fails (a table cell taller than a page), the
/// protocol is printed again as plain lines, so there is always a PDF.
pub fn render(title: &str, subtitle: &str, markdown: &str, font_dir: &Path) -> Result<Vec<u8>> {
    let look = Look::for_text(&format!(
        "{title}\n{subtitle}\n{}",
        outline_cleanup(markdown)
    ));
    let blocks = parse(markdown);
    let mut doc = document(title, subtitle, font_dir, look)?;
    for b in &blocks {
        doc.push(Boxed(element(b, look)));
    }
    let mut out = Vec::new();
    match doc.render(&mut out) {
        Ok(()) => Ok(out),
        Err(e) => {
            tracing::warn!("PDF layout failed ({e}), printing the plain text instead");
            let mut doc = document(title, subtitle, font_dir, look)?;
            for line in outline_cleanup(markdown).lines() {
                doc.push(elements::Paragraph::new(look.text(&printable(line))));
            }
            let mut out = Vec::new();
            doc.render(&mut out)
                .map_err(|e| anyhow::anyhow!("PDF: {e}"))?;
            Ok(out)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spans(text: &str) -> Vec<Span> {
        vec![Span {
            text: text.into(),
            ..Default::default()
        }]
    }

    #[test]
    fn outline_markdown_becomes_blocks() {
        let md = "## Agenda\n\n* Müll (Ben)\n* Kaffee\n\n➕ New items from the chat (!top) go here.\n\n\\\n\n:::info\nHinweis\n:::\n\n- [ ] Anna: Kompost\n- [x] erledigt\n\n| A | B |\n|---|---|\n| 1 | 2 |\n";
        let blocks = parse(md);
        assert_eq!(blocks[0], Block::Heading(2, spans("Agenda")));
        let Block::List {
            ordered: false,
            items,
        } = &blocks[1]
        else {
            panic!("{blocks:?}")
        };
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].blocks, vec![Block::Para(spans("Müll (Ben)"))]);
        // the marker, the backslash line and the notice fences are gone
        assert_eq!(blocks[2], Block::Para(spans("Hinweis")));
        let Block::List { items, .. } = &blocks[3] else {
            panic!("{blocks:?}")
        };
        assert_eq!(items[0].check, Some(false));
        assert_eq!(items[1].check, Some(true));
        let Block::Table(rows) = &blocks[4] else {
            panic!("{blocks:?}")
        };
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1][1], spans("2"));
    }

    #[test]
    fn bold_and_italic_are_kept_per_span() {
        let blocks = parse("**Beschluss:** wir *machen* das");
        let Block::Para(s) = &blocks[0] else { panic!() };
        assert!(s[0].bold && !s[0].italic);
        assert!(s.iter().any(|x| x.italic && x.text == "machen"));
    }

    #[test]
    fn the_cheap_fonts_are_used_where_they_have_every_character() {
        assert!(Look::for_text("Müll, Straße – „Beschluss“ € ☐ 📋").builtin);
        assert!(!Look::for_text("Привіт").builtin);
        assert!(!Look::for_text("Łódź").builtin);
        assert_eq!(Look { builtin: true }.text("☐ a ☑ b"), "[ ] a [x] b");
        assert_eq!(Look { builtin: false }.text("☐ a"), "☐ a");
    }

    #[test]
    fn emoji_are_left_out_but_checkboxes_stay() {
        assert_eq!(
            printable("📋 Plan ✅ ☐ ☑ äöüß Привіт"),
            " Plan  ☐ ☑ äöüß Привіт"
        );
    }

    #[test]
    fn a_protocol_renders_to_a_pdf() {
        let dir = ["/usr/share/fonts/truetype/dejavu", "/usr/share/fonts/TTF"]
            .into_iter()
            .map(Path::new)
            .find(|d| d.join("DejaVuSans.ttf").exists());
        let Some(dir) = dir else {
            eprintln!("no DejaVu fonts here, skipped");
            return;
        };
        let md = format!(
            "## Tagesordnung\n\n* Müll\n  * Tonnen\n\n| A | B |\n|---|---|\n| {} | 2 |\n\n```\ncode\n```\n",
            "lang ".repeat(200)
        );
        let pdf = render("House plenum 2026-11-01", "As of 2 Nov 2026", &md, dir).unwrap();
        assert!(pdf.starts_with(b"%PDF"));
        // Western European only (umlauts included): the reader's fonts, nothing embedded.
        assert!(pdf.len() < 100_000, "{} bytes", pdf.len());
        // Cyrillic needs DejaVu embedded.
        let pdf = render("House plenum", "As of", "Привіт, сусіди", dir).unwrap();
        assert!(pdf.len() > 500_000, "{} bytes", pdf.len());
    }
}
