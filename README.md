# 📋 plenum-bot

Plenum protocols in [Outline](https://www.getoutline.com): for every meeting of a plenum the
bot creates the protocol from the plenum's template in the wiki, posts the link in the
plenum's room and puts `!top` items from the chat on the agenda. Afterwards it copies every
line starting with "Decision:" into the plenum's decision list, attaches a PDF and posts a
summary. Searching happens in Outline. Several plenums (house, kitchens, a working group),
each with its own room, collection and Nextcloud calendar.

A plenum may belong to a directory group (`group`): its room's members follow the group
(`[directory]`, `[members]`: who is in it is invited once, who is not is removed; the house's
bots and the room's admins never), and the group writes in its collection (`access`: `house`
everyone writes, `members` everyone reads and the group writes, `private` only the group).
A group that exists with nobody in it empties its room (but for those never removed); a group
the directory does not know, or a directory that cannot be read, leaves the room as it is.
Only who lives here counts in a team's group: a plenum with access `members` or `private`
requires the house plenum's group (access `house`, e.g. `residents`) by default
(`require_group`, `""` for none), so a former resident still in a kitchen's group is neither
kept nor invited. The directory token needs to view users and groups.

The dates are the plenum's Nextcloud calendar (`[calendar]`; one per plenum, `plenum-<key>`,
owned by the bot's own Nextcloud account, which makes it and shares it by `access`: the house
group writes the house's, a group writes its own and the house reads it, a private one only
its group; `readers` such as calendar-bot read every one that is not private). People make,
move, cancel and repeat meetings there; the bot reads it every `refresh_minutes` over CalDAV.
A meeting is the calendar's occurrence, found again by its ID (UID, for a series with its
original date): moved, it keeps its protocol (renamed, announced again); gone before it began,
it is cancelled (said once in the room). A room that knew the old date (or that it was
cancelled) is told at once when it moves or comes back. While the calendar cannot be read, or
answers anything but a WebDAV multistatus, nothing changes; a calendar the bot had to make
anew is read one round later. Calendar times are read in `[defaults] timezone`, whatever the
container's `TZ`.

## Commands (in a plenum's room)

| | |
|---|---|
| `!plenum` | next date and the protocol's link |
| `!top <topic>` | an item for the agenda (waits for the protocol if it does not exist yet) |
| `!minutes done` | close the protocol now (decisions, PDF, summary). While the meeting is on only `organizers` and the admins; after its end everyone (or `organizers` only, if set) |
| `!minutes confidential` | nothing from this meeting's minutes comes into the chat (see below) |
| `!minutes link` | only the link to them |
| `!minutes full` | the summary again, as usual |
| `!top` as a reply | the message it answers is the topic (by its author) |
| `!plenum help` | this list |

Several plenums in one room: the key goes along, `!top kitchen-floor-1 dishwasher`. Commands
older than 15 minutes (a fresh Matrix store syncs the rooms' history) are left alone.

## What goes into the chat

Per meeting, set in its room; for a plenum by `publication` in its `[[plenum]]` (default
`[defaults] publication`, `full`). The minutes are made, filled from `!top`, closed, filed,
attached as a PDF and their decisions listed in the wiki the same way in every mode; only
what the room gets differs:

| | `full` (default) | `link` | `confidential` |
|---|---|---|---|
| announcement, reminder, cancellation | with the link | with the link | when, no link, no item count |
| answer to `!top`, `!plenum` | with the link | with the link | without it |
| when closed | summary: decisions, open tasks, mentions; the PDF | that it is closed, the link | that it is closed |

`!minutes confidential|link|full` is for the meeting whose minutes are open (it began and is not
closed yet), else the next one; another one by its day, `!minutes confidential 14.11.` (also
`14.11.2026`, `2026-11-14`, and `14.11. 19:00` when two are on one day). The answer names the
meeting. Everyone in the room may share less; sharing more again is for the `organizers`, the
admins and whoever set it. The setting is saved with the meeting before the answer, kept when
it moves in the calendar, and read by every step when it sends: a close that failed, or a
restart, sends what the meeting is set to now. What went out already stays (an announcement
with the link, say); a closed meeting cannot be changed any more. Open tasks of a meeting that
was not shared in full stay out of the summaries of later meetings they come along to (as long
as their text is unchanged). The overview in the wiki marks such meetings.

## In Outline

Per plenum, in its collection: "<Name>: Overview" (the bot writes it anew when something
changes: next dates, how to bring in a topic, the calendar link, open tasks, latest
decisions), "<Name>: Rules" (what holds now; people keep it), "<Name>: Template" (the
template, created on first use, editable; `{{plenum}}`, `{{weekday}}`, `{{date}}`, `{{time}}`,
`{{iso_date}}` are filled in, `{{open_tasks}}` takes the open tasks of the last protocol),
"<Name>: Decisions" (the decisions, newest at the bottom) and "<Name>: Minutes" with one
"<Name> YYYY-MM-DD" per meeting below it. The collection's access is checked every hour and
put back if changed. Open tasks of a closed protocol also go into the next one when that was
made already (meetings close together). The line "➕ New items from the chat (!top) go here." marks where chat
items go.

A plenum's collection and documents are found again by their IDs: when its name changes, they
are renamed, not made anew; a document nobody changed since the bot made it gets the new text
too. A private collection is only ever found by its ID, never taken over by its name.

API key: the key of an Outline account of its own, as `OUTLINE_API_KEY`, with the scopes
`documents.* collections.* attachments.* files.* auth.info` (stored by Outline as
`/api/documents.*` and so on). A member is enough: it creates its collections itself, readable
and writable by every member (tested against Outline 1.11.0, 2026-10-20). In house-infra the
account and key come from stacks/outline (outline-roles, outline-keys).

## Robustness

`store/state.json` records every step of every meeting when it is done; messages carry a
transaction ID saved before sending; the protocol's ID is chosen and saved before it is
created. A restart at any point neither repeats nor skips a step; a step whose state cannot
be saved stops before its side effect. A failing meeting is retried
every minute and holds up no other.

The bot never rewrites a protocol people write in: a chat item replaces only the marker line
(Outline's `patch`), everything else is appended. Outline's Markdown does not survive a full
rewrite of every text (a literal `[x]` comes back as a checkbox). Chat items carry no
checkboxes and no "Decision:", and the agenda is never read as decisions.

## Tests

```
cargo test
OUTLINE_IT_URL=http://localhost:3000 OUTLINE_IT_KEY=ol_api_… cargo test -- --ignored a_meeting_in_a_real_outline
```

The second runs a whole meeting against a real Outline (it creates a collection "IT <uuid>").
