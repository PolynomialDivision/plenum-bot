# 📋 plenum-bot

Plenum protocols in [Outline](https://www.getoutline.com): for every meeting of a plenum the
bot creates the protocol from the plenum's template in the wiki, posts the link in the
plenum's room and puts `!top` items from the chat on the agenda. Afterwards it copies every
line starting with "Decision:" into the plenum's decision list, attaches a PDF and posts a
summary. Searching happens in Outline. Several plenums (house, kitchens, a working group),
each with its own room, collection and Nextcloud calendar.

A plenum may belong to a directory group (`group`): its room's members follow the group
(`[directory]`, `[members]`; mxbot-common `membership`, house-infra docs/HOUSE-DIRECTORY.md),
and the group writes in its collection (`access`: `house` everyone writes, `members` everyone
reads and the group writes, `private` only the group; "everyone" is the house plenum's group
in the wiki, "In Outline": the house group). Per room, from one read of the
directory:

- **invited**: the account each member *chose* for the house (`matrix_preferred`: their house
  account or a link they verified), once; who leaves on their own is not invited again while
  the account stays theirs, a failed invitation is tried again a day later. While another
  account of the member is in the room (a handover is due) and the chosen one is not, it is
  invited once more per handover, even if it was invited before (declined, or joined and
  left: an older bot's invitation, say). Nobody who has not
  chosen is invited: with `ask_to_choose` the bot writes to them (house account and verified
  links, German, at most three times, two weeks apart) where to choose (`choose_url`);
- **kept**: one account per member, the one they chose; while they have not chosen (or their
  choice cannot be used), every account the directory knows as theirs (the house account,
  their links in `matrix_ids`). The house's bots and `keep`, and the room's admins
  (power level 100), always;
- **handed over**: a member's other accounts in the room once they chose one (the house
  account that joined on its own, an older choice, a link an admin entered) stay until
  `handover_days` (7) after the bot first saw the chosen account *joined* there, at most
  `handover_max_days` (30) after it first saw the choice there (each at most 365); then they
  are removed like anyone else's (counted against the limit below, held the same way), with
  the reason "Ersetzt durch dein gewähltes Konto @… (Einstellungen → Matrix-Konten)", but only
  while the chosen account is in the room (joined, or invited): a handover never removes a
  member's last account there. When the chosen account is neither (its invitation failed, or
  it declined or left after the handover's own invitation) the other accounts stay, as the
  member's recognised accounts they are, and `!members` says why. Once per choice the bot
  writes to the chosen account (German: which account stays, which ones go by when, in which
  rooms, whether an invitation waits for it or it is not in a room yet, where to change it;
  whatever `ask_to_choose` says, since this follows the member's own choice); if that fails,
  `!members` still lists it. A new choice starts a new
  handover, and an account given up is invited again when it is chosen again. The first
  round with this image starts every handover then, so nobody goes before the grace; who
  already chose a link while the house account is still in a room hears of it then;
- **removed**: everyone else (a kick; the bot never bans, deactivates or changes power levels);
- **left alone**: an account two people claim, in a room where one of them belongs (neither
  invited nor removed); where none of them belongs it goes like anyone else's. The admins hear
  of each such account once (who claims it).

Nothing changes while the directory cannot be read or its last good read is older than
`[directory] stale_after_minutes` (60), nor when the house plenum's group (access `house`, e.g.
`residents`) exists with nobody in it; a room whose group the directory does not know is left
as it is. More removals in one room than 20 % of its people (at least 2) are held: invitations
still go out, the admins hear of it once per plan in their direct chats with the bot, with the
plan's digest (8 hex digits over its accounts), and `!members confirm <plenum key|room ID|alias>
<digest>` there carries that very plan out once and answers with the accounts it removed;
another digest (or none) confirms nothing and shows the plan held and its digest (a changed plan
is held anew; a plan that shrinks within the limit goes by itself). The admins also hear once
about a member whose choice cannot be used (not theirs, unverified, claimed twice), about an
account two people claim, and about a deactivated person removed from a room (their Matrix
account is still active: offboarding is a human step). They get one list of everyone who cannot
be invited (not chosen, or an unusable choice: their username and the rooms they would be
invited to) and again whenever someone is added to it; with `ask_to_choose` off nobody else
tells those people, so switch it on together with this image. Every notice's transaction key
names its episode (the hold's digest and since when, or the day it was found): the homeserver
drops a key it has seen before, so a recurring notice would otherwise be lost. Only who lives here counts in a team's group: a plenum with access `members` or
`private` requires the house plenum's group by default (`require_group`, `""` for none), so a
former resident still in a kitchen's group is neither kept nor invited. The directory token
needs to view users and groups.

**Subtenants.** People who live here for a while (house-infra's `subtenants` group,
docs/HOUSE-DIRECTORY.md) get their own kitchen and nothing of the house plenum. With
`[members] subtenant_groups = ["subtenants"]` a kitchen (access `members`) that requires the
house plenum's group takes its members in one of these groups too: they are invited with their
chosen account (asked to choose like everyone, may link accounts and write `!link`, as members
of a managed group, also while they are only in `subtenants` and their kitchen's group comes
later), kept, and removed once they are in neither (moved out to
`former-residents`). The house plenum's room follows its own group (`residents`) alone, and a
private plenum (finance) keeps requiring the house's group only, so subtenants are never
invited there; who is invited there by hand is removed. A subtenant group the directory does
not know leaves the kitchens' rooms alone (logged), as any unknown group. Their calendars: a
kitchen's is edited by its group and read by the house group (`residents`), the house plenum's
by the house group only, so a subtenant edits their kitchen's calendar and sees no other
(`calendar.rs`). Unset (the default): only the house plenum's group counts, as before.

**The end of a stay.** An admin may set `stay_until` ("YYYY-MM-DD"; `[directory]
stay_until_attribute`) on anyone in the directory, typically a subtenant. The admins hear of
it in their direct chats with the bot `[members] stay_reminder_days` (14, 1–365) before it, on
the day (or the day after, when the bot was down that day), and once more when the person
still lives here more than a day after it ("overdue"); each once per person and date (kept in
`store/state.json`, `stays`, so a restart tells nothing twice), only the stage it is in now
(a reminder missed while the bot was down is not sent late). A changed date starts again; a
removed date, a deactivated or a deleted person is forgotten. The day is the bot's
(`[defaults] timezone`). `!members` lists the stays that end within those days and the overdue
ones. A stay is over once the person is in none of the living groups (the house plenum's group
and `subtenant_groups`; moved out to `former-residents`, their user still active): no reminder,
not listed. Nothing else follows from it: nobody is locked out or removed; the directory's groups
decide that. An unreadable value is ignored (debug log), never a failed read.

Mentions in the summaries (`@Name` in the wiki) ping the person's chosen account, else their
house account: a mention is no invitation.

**Linking another account.** From an account on another server, `!link <username>` in a direct
chat with the bot answers with the link code for that house user and that account
(mxbot-common `link`), to be entered in the identity provider's flow (`choose_url`) while
logged in as that user. House accounts are refused, five codes per account and hour, old
messages are ignored, and the code is never logged. The key is `MATRIX_LINK_KEY` in the
environment; without it the bot answers that linking is not set up.

**Link requests from the settings.** A person who names an account in the flow without a code
gets a link request (`[directory] link_request_attribute`, `matrix_link_request`: `{"id",
"at"}`). With `[directory]` and `MATRIX_LINK_KEY` the bot reads the requests every 20 s
(Authentik filtered to the users with one), and for each fresh one (younger than 30 minutes)
opens a direct chat with that account (an existing private one, else a new encrypted one
inviting it) saying which house login asked ("🔗 Die Haus-Anmeldung `<login>` möchte dieses
Matrix-Konto `…` verknüpfen"), and once the account has joined sends the code of the current
slot, the same as `!link` gives. The account may be anyone's, so the messages name only the
login (as code, which the person cannot change), never the name they show (which they edit
themselves). Only the people in the groups whose rooms the bot manages (the plenums' `group`
and `require_group`) may ask: a guest's or a former resident's request is not read. Not
served: an account on the house's server, one linked to the person already, one another
active person claims (from the bot's own full read of the directory every `refresh_minutes`),
a request withdrawn or replaced meanwhile. At most 5 deliveries per person a day, 3 a day to
one account (whoever asks) and 30 an hour in all (more are skipped: logged, counted in the
status file); an account that has not joined within 30 minutes, or Matrix failing 5 times, is
given up, never retried (the person asks again). Every request taken up is in
`store/state.json` (`link_requests`, a day long, saved before the invitation), so a restart
sends nothing twice; refused and skipped requests are one record per person (the newest, with
how many it stands for), so a script asking every minute does not grow the file. The code is
in the message only. Status file: task `link requests` (the directory down, unreachable,
timing out or answering 5xx/429, is `unavailable`, never unhealthy; a refused token or an
answer the bot cannot read is `failed`, unhealthy after its time), queue `link_codes` (waiting
for their account), gauges `link_codes_skipped`, `link_codes_gave_up` (the last day). The same
holds for the task `room members`. When Authentik refuses the link-request filter (400), the
bot reads every active user instead for the rest of its run (one warning).

**Who may open a direct chat with the bot**: the admins, `[security] allowed_inviters` and
members of `dm_rooms` as before, and with link requests served also every account the
directory recognises as an active person's in the groups whose rooms the bot manages (house
account and links, while the bot's last full read is fresh; not a guest's or a former
resident's) and the account of a fresh link request (for 30 minutes). Such a chat is kept only
while it is the bot and that account alone, encrypted; a non-admin can only use `!link` in it.
Direct-chat invites that came while the bot was down wait for its first read of the directory
instead of being rejected. The bot's own room (where `!plenum` shows every plenum) is known by
its room ID, which the bot asks its own homeserver for (the alias `#<bot localpart>:<server>`),
never by the alias a room names itself: anyone who makes a room can set that.

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

In an admin's direct chat with the bot: `!plenum` (every plenum), `!members` (the held
removals and their digests, the running handovers and their end dates, the stays that end soon
or are overdue), `!members confirm <plenum key|room> <digest>`. In a direct chat from an account on another
server: `!link <username>`.

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

**The house group** (2026-10-31, for subtenants): Outline 1.12.0 self-hosted has no guest role
(its Community plan refuses it), so a subtenant in the wiki is a member or viewer, and both read
every collection open to the whole workspace. With the house group (the house plenum's
`group`, `config::wiki_house_group`, given to `Outline::with_house_group` in main.rs) no
collection is open to the workspace: each is private, and the house group gets what "everyone"
got before: `house` read and write, `members` read (the plenum's group read and write, as
before), `private` nothing (taken away when a plenum becomes private). A plenum whose own group
is the house group gives it read and write (no read one hour, read and write the next). The
group gets it before the collection is made private, so who lives here never loses it on the
way; an existing collection is changed in place at the hourly check. While Outline has no group
of that name (house-infra's outline-roles makes it), nothing is opened: an existing collection
keeps its access (a private one stays private), a new one is made private (closed to the house
too until the group exists, then the next check gives it), a plenum made private is made
private; the log says so. Without a house plenum with a group: everything as before. There is
one house group: house plenums with different groups are refused at startup (give them the same
group, or all but one access `members`; before 2026-10-31 that was allowed when every other
plenum set its `require_group`). Tested against Outline 1.12.0
(`house_group_areas_in_a_real_outline`, below; house-infra stacks/outline README "Subtenants").

API key: the key of an Outline account of its own, as `OUTLINE_API_KEY`, with the scopes
`documents.* collections.* attachments.* files.* groups.list auth.info` (stored by Outline as
`/api/documents.*` and so on). A member is enough: it creates its collections itself and is
their admin (tested against Outline 1.11.0, 2026-10-20, and 1.12.0, 2026-10-31). In house-infra
the account and key come from stacks/outline (outline-roles, outline-keys).

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
`house_group_areas_in_a_real_outline` (also `--ignored`) sets up a house plenum, a kitchen and
the finance group first open to the workspace, then with the house group, and checks each
collection's access and groups; it needs an admin's key (`OUTLINE_IT_ADMIN_KEY`) and the
groups in Outline (`OUTLINE_IT_HOUSE_GROUP`, `OUTLINE_IT_KITCHEN_GROUP`,
`OUTLINE_IT_FINANCE_GROUP`).
