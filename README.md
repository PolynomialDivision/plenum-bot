# 📋 plenum-bot

Plenum protocols in [Outline](https://www.getoutline.com): for every meeting of a plenum the
bot creates the protocol from the plenum's template in the wiki, posts the link in the
plenum's room and puts `!top` items from the chat on the agenda. Afterwards it copies every
line starting with "Beschluss:" into the plenum's decision list, attaches a PDF and posts a
summary. Searching happens in Outline. Several plenums (house, kitchens), each with its own
room, collection and rhythm.

## Commands (in a plenum's room)

| | |
|---|---|
| `!plenum` | next date and the protocol's link |
| `!top <Thema>` | an item for the agenda (waits for the protocol if it does not exist yet) |
| `!protokoll fertig` | close the protocol now (decisions, PDF, summary; `organizers` only, if set) |
| `!plenum absagen 27.10.` | cancel a date (`organizers` only, if set) |
| `!plenum extra 30.10. 19:00` | an extra date |
| `!plenum hilfe` | this list |

Several plenums in one room: the key goes along, `!top kueche-eg Spülmaschine`.

## In Outline

Per plenum, in its collection: "Vorlage <Name>" (the template, created on first use, editable;
`{{plenum}}`, `{{wochentag}}`, `{{datum}}`, `{{uhrzeit}}`, `{{iso_datum}}` are filled in),
"Beschlüsse <Name>" (the decisions, newest at the bottom) and one "<Name> YYYY-MM-DD" per
meeting. The line "➕ Neue Punkte kommen mit !top im Chat hier dazu." marks where chat items go.

API key: the key of an Outline account of its own, as `OUTLINE_API_KEY`, with the scopes
`documents.* collections.* attachments.* files.* auth.info` (stored by Outline as
`/api/documents.*` and so on). A member is enough: it creates its collections itself, readable
and writable by every member (tested against Outline 1.11.0, 2026-10-20). In house-infra the
account and key come from stacks/outline (outline-roles, outline-keys).

## Robustness

`store/state.json` records every step of every meeting when it is done; messages carry a
transaction ID saved before sending; the protocol's ID is chosen and saved before it is
created. A restart at any point neither repeats nor skips a step. A failing meeting is retried
every minute and holds up no other.

The bot never rewrites a protocol people write in: a chat item replaces only the marker line
(Outline's `patch`), everything else is appended. Outline's Markdown does not survive a full
rewrite of every text (a literal `[x]` comes back as a checkbox). Chat items carry no
checkboxes and no "Beschluss:", and the agenda is never read as decisions.

## Tests

```
cargo test
OUTLINE_IT_URL=http://localhost:3000 OUTLINE_IT_KEY=ol_api_… cargo test -- --ignored a_meeting_in_a_real_outline
```

The second runs a whole meeting against a real Outline (it creates a collection "IT <uuid>").
