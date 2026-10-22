# 📋 plenum-bot

Plenum protocols in [Outline](https://www.getoutline.com): for every meeting of a plenum the
bot creates the protocol from the plenum's template in the wiki, posts the link in the
plenum's room and puts `!top` items from the chat on the agenda. Afterwards it copies every
line starting with "Beschluss:" into the plenum's decision list, attaches a PDF and posts a
summary. Searching happens in Outline. Several plenums (house, kitchens, a working group),
each with its own room, collection and Nextcloud calendar.

A plenum may belong to a directory group (`group`): its room's members follow the group
(`[directory]`, `[members]`: who is in it is invited once, who is not is removed; the house's
bots and the room's admins never), and the group writes in its collection (`access`: `house`
everyone writes, `members` everyone reads and the group writes, `private` only the group).

The dates are the plenum's Nextcloud calendar (`[calendar]`; one per plenum, `plenum-<key>`,
owned by the bot's own Nextcloud account, which makes it and shares it by `access`: the house
group writes the house's, a group writes its own and the house reads it, a private one only
its group; `readers` such as calendar-bot read every one that is not private). People make,
move, cancel and repeat meetings there; the bot reads it every `refresh_minutes` over CalDAV.
A meeting is the calendar's occurrence, found again by its ID (UID, for a series with its
original date): moved, it keeps its protocol (renamed, announced again); gone before it began,
it is cancelled (said once in the room); while the calendar cannot be read nothing changes.

## Commands (in a plenum's room)

| | |
|---|---|
| `!plenum` | next date and the protocol's link |
| `!top <Thema>` | an item for the agenda (waits for the protocol if it does not exist yet) |
| `!protokoll fertig` | close the protocol now (decisions, PDF, summary; `organizers` only, if set) |
| `!top` as a reply | the message it answers is the topic (by its author) |
| `!plenum hilfe` | this list |

Several plenums in one room: the key goes along, `!top kueche-eg Spülmaschine`.

## In Outline

Per plenum, in its collection: "Übersicht <Name>" (the bot writes it anew when something
changes: next dates, how to bring in a topic, the calendar link, open tasks, latest
decisions), "Regeln <Name>" (what holds now; people keep it), "Vorlage <Name>" (the template,
created on first use, editable; `{{plenum}}`, `{{wochentag}}`, `{{datum}}`, `{{uhrzeit}}`,
`{{iso_datum}}` are filled in, `{{offene_aufgaben}}` takes the open tasks of the last protocol),
"Beschlüsse <Name>" (the decisions, newest at the bottom) and "Protokolle <Name>" with one
"<Name> YYYY-MM-DD" per meeting below it (older protocols at the top are moved there once). The
collection's access is checked every hour and put back if changed. The line "➕ Neue Punkte kommen mit !top im Chat hier dazu." marks where chat items go.

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
