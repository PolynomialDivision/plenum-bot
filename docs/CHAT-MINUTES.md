# Minutes from the chat

Residents often post a plenum's minutes as a message in the room instead of writing them on
the meeting's page in the wiki. With `[minutes] from_chat = true` the bot takes such a message
into the wiki, so the wiki stays the archive and the room stays the place people write in.
Off by default: then nothing here happens and the bot behaves as described in
docs/PLENUM-PAGES.md.

## 1. One record per meeting, two doors

The meeting's wiki page stays the unit of the archive. Minutes reach a meeting either through
its page (as before) or through a chat message. Afterwards the same three things hold: the
minutes can be read in the room, they are in the wiki below the meeting's page, and their
decisions and tasks are in the plenum's lists.

Whichever door the minutes came through is their source of truth, and the bot copies one way
only:

| Minutes came as | Truth | Copy | Later changes |
|---|---|---|---|
| a chat message | the message | a wiki document only the bot writes | an edit or deletion of the message is followed |
| text on the page | the page | one message in the room at close (`publication: full`) | the message is a snapshot; the wiki keeps the history |

A meeting may have both. Nothing is merged then: the page keeps its text, the copy sits below
it, and decisions and tasks are the union of both.

Not done, on purpose: a two-way sync between page and message, a web form or widget, the bot
leading the meeting in the chat, a language model that structures or summarises.

## 2. The copy in the wiki

A document of its own below the meeting's page ("<page title>: Chat minutes"), or, without a
meeting, below "<Name>: Minutes" ("<Name> YYYY-MM-DD: Chat minutes"). The bot replaces its
whole text when a message changes; it never rewrites a page people write in (Outline's
Markdown changes text on a full rewrite, and its API cannot replace a block). The page gets the
link in its result box, or in one appended line when the minutes came after the close.

A copy a person changed belongs to that person: before each write the bot compares the copy
with the fingerprint (`former::fingerprint`, letters and digits) of what Outline gave back after
its last write. A different text, or a copy archived or deleted in the wiki, and the bot stops
writing it for good; the thread hears it once, at the first change of the message that no longer
reaches the copy. (An archived document refuses every write: trying on would fail at every
tick.)

The copy starts with a notice (who posted it, when, the last change, how it is kept), then the
messages, each under its author and time when there are several. A message's text is converted
so it cannot break the page: a chat line break becomes a paragraph break (list items and table
rows stay together), code blocks stay as they are, a line cannot open a notice (`:::`),
mentions keep their label, images become links, links other than http, https and mailto become
text. Every common client sends Markdown source in `body`; `formatted_body` is not read.

## 3. Which messages

A message is taken when

- its first non-empty line starts with "Protokoll", "Plenumsprotokoll", "Minutes" or
  "Protocol" (any case, after `#`, `*`, emoji and spaces), that line is at most 80 characters
  long, and at least three more non-empty lines or 300 characters follow; or
- someone answers it with `!minutes`; or
- it is `!minutes`, a line break and the minutes.

A rule, not a guess: a miss is repaired with one reply, a wrong take with `!minutes undo`. Not
taken by itself: the bot's own messages, `[members] keep`, and what the first sync of a fresh
Matrix store brings back (`mxbot_common::events::is_replayed`). The 15-minute rule of the
commands does not apply to a plain minutes message: one posted while the bot was away is taken
when it is back. At most ten messages per plenum and day.

Several plenums in one room: the key or the name in the first line decides, else the thread
reply asks for `!minutes <key>`.

`!minutes` as a reply reads the message as it shows now (its sender's latest edit, of the newest
ten). An answer to the bot's own thread reply is about the thread's first message, and so are
`!minutes undo` and `!minutes <day>` typed in a thread without answering anything: the thread
reply says to answer there. A bare `!minutes` that answers nothing is the overview, in a thread
too; switched off (`from_chat = false`) it is the overview as a reply as well, as it always was.

## 4. Which meeting

A day in the first two lines (`15.11.`, `15.11.2026`, `2026-11-15`, `15. November`,
`15 Nov 2026`) picks the plenum's meeting on that day (with two that day: the latest that has
started). Without a year it is the latest such day not after today, at most 60 days back; a
day in the future is ignored. Without a day it is the latest meeting that has started, at most
21 days back. Without either the minutes are filed by their day (a spontaneous meeting needs no
calendar entry). Cancelled and skipped meetings are not candidates. `!minutes <day>` as a reply
moves filed minutes to that day's meeting; both copies are written anew.

## 5. What is read

Labels at the start of a line (after a list mark, `#`, emphasis or an emoji), any case,
followed by a colon:

| Kind | Labels |
|---|---|
| decision | Decision, Decisions, Decided, Resolution, Resolved, Agreed, Agreement, Consensus · Beschluss, Beschlüsse, Beschlossen, Entscheidung, Entscheidungen, Entschieden, Einigung, Konsens |
| task | Task, Tasks, To do, To-do, Todo, Todos, Action, Actions, Action item, Action items · Aufgabe, Aufgaben, Zu tun, Zu erledigen, Auftrag, Aufträge; also an open checkbox `- [ ]` |
| postponed | Postponed, Deferred, Adjourned, Tabled, Next time, Next meeting, Carried over, Not reached · Vertagt, Verschoben, Nächstes Mal, Nächstes Plenum, Nicht geschafft, Offen geblieben |
| done | Done, Completed, Finished · Erledigt, Fertig, Abgeschlossen; also a ticked checkbox `- [x]` |

Two forms: one line (`Beschluss: Räder ohne Schild kommen in die Werkstatt.`), or a section
(the label alone, with a colon, as a heading or in bold, then its lines up to an empty line,
the next label or a heading; each is one entry). Who has a task is part of its text. Not
entries: a label with nothing after it, a lone `-`, and an open checkbox under "Done:" (the
template's open tasks nobody ticked), so a template posted back as it is files nothing. A done
line closes the open task with the same text (a ticked checkbox of the template has it), or one
that it is a part of, or that is a part of it, with at least three words; a name alone ("Done:
Anna") closes nothing, it would close every task of that person (`minutes::closes`).
"Result:"/"Ergebnis:" is no decision, a label in the middle of a line is none, code blocks are
not read, topics, attendance and results are not parsed. The table is `minutes::LABELS`: a word
is added there only.

The wiki's readers (`protocol::extract`, `page::topics`) keep their small set of labels:
widening it would read existing pages differently.

## 6. In the room

The bot reacts with 📥 to a message it filed and answers once in its thread (clients without
threads show a normal reply): the meeting, the copy's link (none in a `confidential` meeting)
and what it read, or that nothing was recognised and how to change that; for a kitchen also that
the whole house can read the copy. The reply repeats only what the message says. Later changes
edit that reply, which pings nobody. The latest minutes of the plenum (a resident's message, or
the bot's minutes message from the wiki, whichever meeting is later) are pinned in the room,
with one `m.room.pinned_events` built from the server's current list; the bot unpins only what
it pinned itself. A room that does not let the bot pin is logged once. `!minutes undo` takes
the bot's 📥 off the message again (a deleted message has none any more).

These are answers to something a person did: they are sent in `quiet` plenums too. The
reminder that no minutes came and the template after the reminder are automatic messages and
follow `messages` (`engine::sends`).

## 7. The close

A meeting whose page has no result and no decision does not close empty at its closing time any
more: it waits up to `wait_days` (7) after its end for minutes from the chat, reading its page
at most every 15 minutes. Once, `nudge_days` (2) after the end and only with `messages all`, the
room hears that no minutes came (both ways, the page's link). Minutes that arrive after the end
close the meeting at the next tick; after the wait it closes as before. A page somebody wrote a
result or a decision on closes at the usual time; `!minutes done` closes at once. The next page
opens by the clock as before.

With chat minutes the close takes the page's decisions and tasks plus the minutes', minus the
tasks a done line closes (those are remembered, `done_tasks`, so an unticked checkbox of one on a
later page does not open it again). Of the page's topics without a result only those a
"Postponed:" line names move on; postponed lines that name no topic become topics of their own
on the next page; the others count as dealt with in the chat minutes. The opposite default
(carry everything) is what filled the pages before. The result box gets the copy's link and
names those topics as "in the chat minutes"; the PDF is the page followed by the copy; no
summary, closed note or PDF goes into the room: the minutes are there already.

Minutes that arrive or change after the close (or have no meeting) are applied to the lists:
their decisions replace theirs in the overview, the decision list gets a block when they differ
from what it listed ("(updated <day>, <time>)" after the first, "(withdrawn …)" when the
message is gone), new tasks join the open ones and tasks that left the message leave them, done
lines close them, new postponed topics wait for the next page. The new tasks also go onto the
next page when it is open already (as open checklist items, once): its close takes the open
tasks from it and would drop what it never had. Tasks of minutes filed by a later day than a
meeting stay open when that meeting closes. A deleted message, or `!minutes undo`,
withdraws it: with no message left the copy goes to the wiki's trash (Outline keeps it 30 days)
and its decisions and tasks leave the lists.

## 8. Minutes from the wiki as one message

With `publication: full` and `messages all`, a meeting whose minutes were written on the page
gets them at close as one message instead of the summary: date, the people row of the page
(facilitation, minutes, present), each topic with its result, the decisions, what was postponed,
the open tasks (without those withheld from the chat), the page's link, with the mentions the
summary had. No tables (not every client renders them), checkboxes as ☐. Above 16 000
characters the bot falls back to the summary and the PDF. `link` and `confidential` are
unchanged. The message is a snapshot: a later change of the page does not update it.

## 9. The template

"<Name>: Chat template" in the plenum's collection, made by the bot with a default (English)
and edited by people like the page template; the template is the document's first code block
(so its lines stay lines in Outline). One the bot made and nobody changed follows a new default,
an edited one stays (`written`, as for the page template). Placeholders: `{{plenum}}`,
`{{date}}`, `{{weekday}}`, `{{time}}`, `{{iso_date}}`, `{{topics}}` (one `## <title>` per topic
of the open page) and `{{open_tasks}}` (one `- [ ] <task>` per open task); the last two only for
a meeting shared in full, else left out. A house that writes German edits it once
("Protokoll …", "Anwesend:", "Beschlüsse:", "Aufgaben:", "Vertagt:", "Erledigt:"): every word of
it is in the label table.

The template is posted as a message of its own, nothing before or after it, so "copy" on a phone
copies exactly it: by itself right after the reminder (`template = true`, `messages all`, once
per meeting), and after a one-line answer to `!minutes template`. The automatic one is never
deleted (a deletion leaves a placeholder, and it is often needed a day later); it is edited once
to one line when the meeting's minutes are filed, or when the meeting closes without any. One
asked for with the command is an answer and stays as it is.

Everything the bot says is English; German is only read (the keywords and the labels).

## 10. Configuration and state

```toml
[minutes]
from_chat = false   # take minutes posted in a plenum's room
wait_days = 7       # a meeting without results waits this long for them (0-30)
nudge_days = 2      # one reminder that no minutes came, this long after the meeting (0 = none)
pin = true          # pin the latest minutes message in the room
template = true     # post the fill-in template after the reminder (messages all only)
```

A top-level table (an older image ignores it, as `[pages]`); its keys are checked. What the bot
keeps of chat minutes is `store/minutes.json`, a file of its own beside `state.json` (as
`settings.json`), so a rollback to an older image neither drops nor overwrites it; the older
image closes meetings by the 12-hour rule again. Every step saves before its side effect (the
copy's document ID before it is made, each transaction ID before it is sent), what is left to do
is retried at every tick, and a restart at any saved point repeats and skips nothing. The text of
the taken messages is kept, so a failed write is retried and a restart needs no room history. At
most 60 filings per plenum are kept; older copies stay in the wiki. Message text is never
logged.

## 11. Edge cases

| Case | What happens |
|---|---|
| The bot was down when the minutes were posted | the next sync brings the message and it is taken; after a fresh store it is a replay and skipped, `!minutes` as a reply takes it |
| The bot cannot decrypt the message | the handler never sees it; `!minutes` as a reply reads it once the key is there, else says it cannot read it |
| Minutes in two messages | the first by its first line, the second with `!minutes` as a reply; both in one copy |
| The same text posted again | refused by its fingerprint |
| A corrected version posted as a new message | both are in the copy; the old one is deleted or answered with `!minutes undo` |
| An edit removes the first line's keyword | the message stays taken; only a deletion or `undo` withdraws it |
| An edit by another account, or older than the version kept | ignored |
| Minutes posted before the meeting ended | filed; the close waits for the end |
| The meeting moves in the calendar | page and copy are renamed |
| Someone edits, archives or deletes the copy in the wiki | the copy is theirs, the bot stops writing it |
| Someone deletes the meeting's page | the copy stays; a new filing goes below "<Name>: Minutes" |
| The wiki or Matrix is down | everything is retried each minute; reaction and reply come after the write |
| A kitchen's minutes | the house reads the copy (as its wiki minutes); the reply says so |

## 12. Limits

- The room message of wiki minutes is not updated when the page changes after the close.
- Minutes from bridged rooms or with HTML-only structure are not read.
- A done line in other words than the task's ("Done: tags ordered" for "Anna: order the name
  tags") closes nothing: tick the template's checkbox, or repeat the task's words.
- A meeting whose minutes are prose on the page, without a "Result:" or a decision, counts as
  not written: it waits `wait_days` and is reminded once (`!minutes done` closes it).
- A filing whose copy became the wiki's still takes new messages for the lists, but writes no
  copy for them.

## 13. Tests

The pure parts have table tests (`minutes.rs`: detection, days, meetings, conversion, labels,
texts; `page.rs`, `protocol.rs`: the result box, the people row, the minutes message, the
template). The flows run whole meetings against the fake wiki and chat (`chat_flow.rs`): take,
edit, withdraw, close, late minutes, restarts, the wiki down, `quiet`, `confidential`, pins, the
template, `from_chat` off. The event reading runs on JSON events (`main.rs`). Against a real
Outline: `chat_minutes_in_a_real_outline` (`--ignored`, `OUTLINE_IT_URL`, `OUTLINE_IT_KEY`).
