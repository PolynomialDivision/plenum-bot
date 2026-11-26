//! The texts the bot wrote into its documents in earlier versions. A document that still has
//! one of them is the bot's, not changed by anyone: it gets the bot's newest text. (Outline's
//! "untouched", updatedAt equal to createdAt, is lost with the bot's own first renewal: the
//! templates of 2026-10-22 renewed on 2026-10-23 kept their old text on 2026-11-07.)
//!
//! When a text in `protocol` changes, its old version goes here.

use crate::config::Plenum;

/// A text compared by its letters and digits only: Outline gives a document back in its own
/// Markdown (escapes, list marks, table spacing), the words stay (checked against Outline
/// 1.12.0, 2026-11-07). FNV-1a, stable across Rust versions (it is kept in the state).
pub fn fingerprint(text: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for c in text.chars().filter(|c| c.is_alphanumeric()) {
        for l in c.to_lowercase() {
            let mut b = [0; 4];
            for byte in l.encode_utf8(&mut b).bytes() {
                h ^= u64::from(byte);
                h = h.wrapping_mul(0x0000_0100_0000_01b3);
            }
        }
    }
    h
}

/// The template's former texts (`protocol::DEFAULT_TEMPLATE`).
pub fn templates() -> Vec<String> {
    vec![
        TEMPLATE_2026_10_22.to_owned(),
        TEMPLATE_2026_10_23.to_owned(),
    ]
}

/// The example minutes' former texts (`protocol::example_doc_text`).
pub fn examples(plenum: &Plenum) -> Vec<String> {
    vec![example_2026_10_23(plenum)]
}

/// 9a98fef to d7075bd
const TEMPLATE_2026_10_22: &str = "\
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

/// f3eed1f to ab826dd (deployed until 2026-11-07)
const TEMPLATE_2026_10_23: &str = "\
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
Tasks as a checklist (type \"[]\" at the start of a line in Outline): who does what by when; \
type \"@\" and pick the person, they are told in the wiki and in the plenum's chat. What \
stays open here is in the next minutes again.

{{open_tasks}}
";

/// d7075bd to ab826dd
fn example_2026_10_23(plenum: &Plenum) -> String {
    format!(
        "_An example of finished minutes, to copy from. The real ones are under “{protocols}”; \
         the bot makes them from the template before each meeting._

**{name}** on Sunday, 1 November 2026 at 19:00

| Facilitation | Minutes | Present |
|---|---|---|
| Anna | Ben | Anna, Ben, Cara, Dan |

## Agenda

* Bins in the yard (Ben)
* Compost bin (Cara)

## Minutes

### Bins in the yard

The yellow bin is often full by Thursday. A second one would fit next to the door.

Decision: we ask the landlord for a second yellow bin.

### Compost bin

Cara found one for 40 €. Everyone agrees, Dan would rather have a bigger one next year.

Decision: we buy the compost bin from the house cash box.

## Decisions and tasks

- [ ] Ben: write to the landlord about the second bin by Friday
- [ ] Cara: buy the compost bin by next Sunday
- [x] Dan: put up the new cleaning rota

_After the meeting the two lines starting with “Decision:” go into the list of decisions. Ben's \
and Cara's tasks are still open, so they come along to the next minutes; Dan's is ticked off \
and done._
",
        name = plenum.name,
        protocols = plenum.protocols_title(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fingerprint_sees_only_the_words() {
        assert_eq!(
            fingerprint("**A** \\[x\\] - Decision: \"yes\"\n\n| a | b |"),
            fingerprint("* a [x]\n* decision yes\n|a|b|")
        );
        assert_ne!(fingerprint("Decision: yes"), fingerprint("Decision: no"));
        assert_eq!(fingerprint("Ä"), fingerprint("ä"));
    }

    #[test]
    fn the_former_texts_are_not_the_current_ones() {
        let p = crate::protocol::tests_plenum();
        let now = fingerprint(crate::protocol::DEFAULT_TEMPLATE);
        assert!(templates().iter().all(|t| fingerprint(t) != now));
        let now = fingerprint(&crate::protocol::example_doc_text(&p));
        assert!(examples(&p).iter().all(|t| fingerprint(t) != now));
        assert!(templates()[1].contains("## Agenda"));
        assert!(examples(&p)[0].contains("## Agenda"));
    }
}
