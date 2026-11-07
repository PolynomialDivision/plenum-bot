# Plenum pages: preparation and minutes in the wiki (2026-11-07)

Replaces the room-centred topics of commit 8a0f700 (its docs/TOPICS.md: numbered topics with a
bot message, thread, 👍/🤝, outcomes in threads), which stays in the history.

Guiding idea: **less bot, better plenum pages, easier participation.** The plenum's page in the
wiki is where a meeting is prepared, held and read afterwards. The bot opens it, carries over
what was not reached, writes a short result at its top and, if a plenum wants, posts nothing in
the room but the page's link.

## 1. What we had (inventory)

| Area | Base (ab826dd, deployed) | Room topics (8a0f700, never deployed) |
|---|---|---|
| Page | "<Name> YYYY-MM-DD" under "<Name>: Minutes", made `prepare_days` (3) before from the wiki document "<Name>: Template" (people may edit it; an untouched one follows the bot's default) | the same plus a `### #12` section per topic |
| Topics | `!top <text>` puts a line at a marker under "## Agenda" (or waits in `pending_tops`) | numbered topics, a bot message each, thread, reactions, edits, outcomes, `topics.json` |
| Results | `Decision:` lines, open checkboxes = tasks; at close: decisions list, PDF, summary, open tasks into the next page | + an outcome per topic in its thread, carry-over |
| Room | announcement, reminder, cancellation/move, summary + PDF, answers | + topic messages, 👍/🤝, edits, outcomes, setting changes |
| Settings | house-infra only | `!plenum set` (topics, agenda_days, reminder, carry_over, tell_interested) |

Outline 1.12.0 (checked in its source; house-infra `stacks/outline`): only at home and through the
VPN (owner decision 2026-10-28), share links off, no SMTP (notifications in the app only). The
editor has headings, checklists, tables, notices (`:::info`), mentions; a page's headings are its
contents (side panel on a desktop, contents menu in the header on a phone). `documents.update`
has `replace`, `append`, `prepend`, `patch` (`findText`, first match; a missing text is a 400);
API edits merge with open editors (saved by the browser 3–10 s late). History per document, the
trash keeps deleted documents 30 days. Comments exist; the bot's key has no `comments.*` scope.
`documents.info` returns Outline's own Markdown: lines without an empty line between are one
paragraph; Shift-Enter comes back as `\n` (backslash, n) inside the line.

## 2. Variants and decision

| | Idea | Verdict |
|---|---|---|
| V0 | keep the room topics | no: many messages, commands to learn, a second record beside the wiki, ~5700 lines with tests |
| V1 | a better template and a quieter bot, nothing new read | nearly enough, but a topic not reached is lost unless someone copies it |
| **V2** | V1 + the bot reads the topics at close: carries those without a result onto the next page, writes a result box at the top | **chosen**, in the slim form the reviews asked for (§10) |
| V2+ | V2 + an agenda list in the page the bot keeps in step | no: the bot would write into a live page all the time; the contents menu lists the headings |
| V3 | a standing "topics" page, copied into each meeting's page | no: two places; the early opening (§4.1) makes the next meeting's page the standing one |
| V4 | discussion in Outline comments, read by the bot | no: a new API scope, not in the PDF, easily missed on a phone (comments stay usable, unread by the bot) |

Is V2 simpler and more pleasant than V0? For upkeep clearly (the topics code goes; what comes
is a pure parser and two steps in the existing close). For preparers and minute-takers yes: one
place, nothing written twice, no commands. For a resident away from home without the VPN it is
worse: the wiki does not open (§8). The chat side therefore keeps `!top` as the easy way in and,
in `all` mode, the summary.

## 3. Principles

1. One page per meeting: preparation, working copy, minutes. A topic's preparation and its
   result sit together.
2. Writing in the wiki needs no commands; `!top` stays as a shortcut from the chat.
3. The bot writes into a page only at its edges: when it creates it, at its markers (topics from
   the chat, carried topics, open tasks) and once at close (the result box). It never changes
   what people wrote. Every write is idempotent and checked at a later tick.
4. What the bot posts by itself is one setting per plenum; the code tags every automatic
   message and one function decides. Answers to a command are not automatic messages.
5. Confidentiality, publication and the calendar keep their rules.

## 4. The page

### 4.1 When it opens

"Over" below: the plenum's previous meeting (the latest earlier one neither cancelled nor
skipped) is closed (`!minutes done`) or past its closing time (`end + close_after_hours`).
Time-based, so a failing close does not hold the next page back.

- **`page_after_days` = N** (`[pages]`, per plenum, the room's `page_after`; added 2026-11-07):
  the page opens once the meeting before is over and the local day N days after that
  meeting's end has begun (N = 0: as soon as it is over). So it is there for the whole time in
  between, however far away the meeting is (the next meeting is always in view, not only
  `prepare_days` ahead). With N = 1 and a meeting 19-21 h closed by `!minutes done`: the next
  page at midnight; closed by the clock (12 h): at 09:00 the next day.
- **unset** (the default, behaviour as before), and for a plenum's first meeting (no meeting
  before): at `start − prepare_days`, once the meeting before is over.
- Either way a page opens a day before its meeting even while the one before is open (two
  meetings close together), and a page once open stays open.

One rule decides, never both: with `page_after` set, `page_days` only matters for a first
meeting (`!plenum settings` says so). A cancelled meeting is not "the meeting before": when
the next meeting is cancelled, the one after it opens at once (its previous is long over),
and the cancelled page's topics reach it at the cancelled meeting's end. Considered and left:
`prepare_days 60` alone (the same "right after", no delay possible, misleading texts).

Calendar edits that the long-open page meets more often now:
- a move keeps the page (renamed, announced again);
- a series changed "this and all future" in Nextcloud becomes a new event with new IDs: an open
  page of a meeting still to come whose ID left the calendar takes the single occurrence on its
  day that is new to the bot (a move if the time changed), instead of being cancelled while a
  second page opens. Accepted: a meeting deleted on purpose and another one added that day in
  the same 5 minutes keeps the page (moved, not cancelled); read between the two writes of
  such a change, the meeting is cancelled first and back on at the next read;
- an event entered twice (two IDs, one start) is one meeting; an occurrence the bot keeps
  something of (a page, a publication set with `!minutes … <day>`) is never dropped;
- two meetings at one time are ordered by their ID, so a carried topic goes to one only.

### 4.2 Its shape (the default template; each plenum may change its own in the wiki)

`protocol::DEFAULT_TEMPLATE`, filled in:

```markdown
**House plenum** · Sunday, 16 November 2026, 19:00

➕ The bot writes the result here after the plenum (please leave this line).

| Facilitation | Minutes | Present |
|---|---|---|
|  |  |  |

## Topics

:::info
- **A new topic:** type ### and a space, then its title (or the small heading from the + menu).
- Below it: what it is about; what should come of it (information, a discussion or a decision); your proposal, if you have one.
- Thoughts and questions: as points under the topic, with your name.
- As its last line: Result: (filled in at the plenum; a decision on a line of its own that starts with the word Decision and a colon).
- A topic whose result stays empty comes onto the next page. Not needed any more? Delete it.
- From the chat: !top and your topic.
:::

➕ Topics from the chat are added above this line (please leave it).

## Tasks

A task is a checklist item (type [] and a space): who does what by when; type @ and pick the
person, Outline shows them a notice. What stays open comes onto the next page.

### Open tasks from the last plenum

- [ ] Ben: write to the landlord (since 1 Nov 2026)
```

A plenum that writes its template in German keeps the two ➕ lines word for word (the bot finds
them by their text); "Themen", "Aufgaben", "Ergebnis:" and "Beschluss:" are read too.

A topic, as `!top` writes it (a person types the same, or less):

```markdown
### Washing machine in the basement

**From:** Anna (from the chat)

It has been broken for two weeks; the repair costs 250 €.

Proposal: a new one, up to 400 €.

- Ben: repair it once more? It is 12 years old.

**Result:**
```

### 4.3 Reading a page (src/page.rs)

- Lines: Shift-Enter breaks (`\n` inside a line) split a line; a line that is only `\` is
  empty (also in the decision/task extraction, where it was a bug).
- The topics: the headings after "Topics"/"Themen" up to a heading titled exactly "Tasks",
  "Aufgaben", "Open tasks" or "Open tasks from the last plenum" (or the end). The first
  heading's level is the topics' level: a heading at that level or above starts a topic, a
  deeper one belongs to the topic above it. Notices (the help), the result box and the bot's
  marker lines are skipped.
- A topic's result: the text after its line "Result:"/"Ergebnis:" (bold, a list item, the
  result on the same line: all fine). **Decided**: a decision line in the topic ("Decision:" /
  "Beschluss:", also "Result: Decision: …"). Otherwise: empty after "Result:" **not reached**,
  something after it **discussed**. No "Result:" line and no decision: **no result written**,
  carried like not reached (nothing is lost; the copy says so, and is deleted if not wanted).
  Two "Result:" lines: **to check** (not carried, named in the box).
- Decisions and tasks are found as before (a decision line anywhere outside the agenda, open
  checkboxes); the result box and the template's help (`:::info`) are skipped.
- Pages without a "Topics" heading (old pages, edited templates) have no topics: as before.

### 4.4 After the plenum (steps of the existing close)

1. Decisions, tasks and topics are read.
2. **Carried topics**: those not reached or without a result go into the state with their
   preparation (open checkboxes become plain points, mentions plain names, a "Carried over"
   line replaces an older one, the heading written the way `!top` writes one) and an empty
   "Result:". From there they go into the next page that is open (above its topics' marker,
   else above its "Tasks" heading, else at its end) or into it when it is made, and leave the
   state only when a later read shows them there (their heading compared without emphasis or
   escapes: Outline drops the escape of `_`, checked against 1.12.0). A meeting that receives
   a topic from one shared less (link, confidential) takes that publication (who may share more
   again may raise it with `!minutes full` as before).
3. **Result box** in place of the result marker, once (a page without the marker, e.g. from an
   edited template, gets none: the bot writes only at its markers):
   ```markdown
   :::success
   **Result of the plenum** (filled in by the bot when the minutes were closed)

   - ✅ Washing machine in the basement: we buy a new one, up to 400 €.
   - 💬 Discussed: Garden party
   - ⏭️ On the next page: Bike cellar (not reached)
   - 📌 Tasks: Anna: order the machine by 30 Nov
   :::
   ```
   A decision outside any topic is "✅ Decided: …"; two "Result:" lines give "❓ Two
   "Result:" lines, so not carried over (copy it to the next page if it is still open): …".
   Written, then checked at the next tick before the PDF is made (an API edit can be lost in a
   race with a browser's save; it is written again only if it is missing, at most 3 times).
4. Decisions list, PDF (with the box, without the help and the marker lines), summary in the
   room: as before, if the plenum's `messages` lets it.

A cancelled meeting's page gets "(cancelled)" in its title at once (taken back if it returns).
Once its time is over, all its topics move on as in step 2 (the meeting did not take place) and
it is closed without anything else.

### 4.5 Discussion before the plenum

Points under the topic, with names (seen at the plenum, kept in the minutes); Outline comments
for side remarks (they notify the people mentioned, in the app; not in the PDF); the room as
always. Outline's history shows who changed what and restores a version; deleted pages stay in
the trash 30 days. Several people writing at once is Outline's live editing.

## 5. Messages

One setting per plenum, `messages`:

| Message (tag in the code) | `quiet` | `all` (default) |
|---|---|---|
| `Link`: the page is open (confidential: today's 🔒 note without the link) | ✓ | ✓ |
| `Moved`, `Cancelled`, back again | the link message is **edited** (no new message) | a new message, as today |
| `Reminder` before the start | – | ✓ (`reminder` minutes, 0 = none) |
| `Summary` after close (full / link / confidential as today) and `Pdf` | – | ✓ |
| `Setting`: a setting changed from an admin's DM | – | ✓ |

- Answers to a command (`!top`, `!plenum`, `!minutes`, …) go where the command was sent, in both
  modes: someone asked. A setting changed in the room is said there as the answer; one changed
  from another room (an admin's direct chat) is said in the plenum's room only with `all`
  (before and after the change).
- A link message posted by an image from before (nothing to edit): in `quiet` a move posts the
  link anew, a cancellation says nothing.
- An edit pings nobody (empty mentions); Matrix keeps a message's earlier versions.
- Inviting and removing members (the directory's rooms) are not messages and are not affected.
- A message left out in `quiet` counts as done: switching to `all` later sends nothing stale.
- The bot sends no reactions, threads or topic messages any more.
- Admin DMs (members, stays) and link codes are not plenum-room messages and are unchanged.
- Every send into a plenum's room carries its tag (`Chat::send`/`send_mentioning`/
  `send_file`/`post`), and each automatic step asks `sends(messages, tag)` before it sends; the
  fake chat of the tests records the tags, and tests run whole meetings (move, cancellation,
  return, close, a setting from a DM, a switch back to `all`) in `quiet` and expect the link
  messages and their edits only. (The tag is not checked again in the Matrix client: a new
  automatic step must ask `sends` too.)
- A house-infra `messages` is a default: the room may change it unless `room_settings` is off.

## 6. Settings

| Layer | What | Who |
|---|---|---|
| technical | rooms, groups, access, calendar, `organizers`, `[pages] room_settings` | IT admins, matrix.yml + `make update-bots` |
| house standard | `[pages] messages`, `page_after_days`, `[defaults]` / per plenum `prepare_days`, `remind_minutes`, `publication` | IT admins, the same file |
| the plenum | in the room `messages`, `reminder` (0–1440 min), `page_days` (= prepare_days, 1–60), `page_after` (= page_after_days, `off` or 0–60); in the wiki its template; per meeting `!minutes full\|link\|confidential` | its `organizers`; without any, everyone in the room; admins always |

`!plenum settings`, `!plenum set <name> <value>`, `!plenum reset <name>`; 10 changes per person
and 30 per plenum a day; kept in `store/settings.json` (a file of its own: an older image after a
rollback neither reads nor drops it). Not in the room, by design: publication (it has its
per-meeting ratchet), access, carry-over (always on: a copy is easily deleted, a lost topic is
not noticed).

Config: a top-level `[pages]` table (the image of before 2026-11-07 ignores it; `[defaults]`
and `[[plenum]]` refuse unknown keys, and so does `[pages]` in an image that knows it) with
`messages`, `room_settings`, `page_after_days` and `[pages.plenum.<key>]`.

### 6.1 Finding the current page

- **Pinned in the collection**: the bot pins each open page of its plenum in the plenum's
  collection (`pins.create`, Outline 1.12: shown above the documents on every tab of the
  collection, for everyone who reads it) and unpins it once closed or cancelled. It unpins only
  what it pinned itself (Outline does not say who pinned): a closed page or another document
  someone pins stays; a pin someone removes is not put back until the open pages change. A page
  it may not pin (deleted in the wiki: Outline answers 403) does not keep the others unpinned. Needs `pins.*` in the bot's key (house-infra stacks/outline `PLENUM_BOT_SCOPE`)
  and the bot as admin of the collection (it is of those it made); without it the bot logs
  once per change and goes on. Several plenums in one collection: each pins its own.
- "<Name>: Overview" lists the next meetings with their pages (as before), `!plenum` in the
  room gives the link, and the link message in the room.
- Considered and left: the collection's description (its "Overview" tab is the default only
  until someone picks another tab, Outline remembers that per browser; a human text there would
  need a marker), the home page's pins (only Outline admins pin there).


## 7. Code

- Gone: the room topics (src/topics.rs, src/engine/topic_flow.rs and tests, the reaction,
  redaction, edit and thread handlers, `Chat::react/reactions`, `[topics]`, `topics.json`, the
  `!top` list and `!top #12 …`).
- New: src/page.rs (pure: lines, topics, results, carried blocks, the box, the `!top` block),
  src/plenum_settings.rs (smaller), the tag and gate, `Chat::post`/`edit` for the link message.
- Fixed on the way: a patch whose text is missing (400 in Outline 1.12) is "not there" instead of
  an error; Shift-Enter lines in the extraction.
- Migration: nothing of 8a0f700 was deployed. Untouched templates follow the new default,
  edited ones keep theirs (no "Topics" heading: no topics, as before). The old marker of pages
  and templates is still found. `pending_tops` become topic blocks.

## 8. Reaching the wiki from outside

The page is the centre now, so being away matters more. Within today's decision a resident gets
`vpn-users` and the Tailscale app against `vpn.scharni.org` (docs/NETWORKING.md). Opening the
wiki from outside the Grafana way (`public_auth` + `forward_auth_outside`: Authentik with the
second factor before Outline, only for requests from outside) is possible within the ecosystem
but undoes the decision of 2026-10-28: a stolen Authentik session would read the wiki from
anywhere, and Outline's API and websocket become reachable from the internet. Left to the owner
(a middle way: an opt-in group for outside access).

## 9. Limits

- A meeting added before an already open page gets its own (empty) page; nothing moves by
  itself.
- `From:` is the sender's display name in the room (as `!top` did before).
- An edit to a closed page after close does not reach a carried copy.
- Outline notifies only in its app; the push channel is the room.
- A section between "Topics" and "Tasks" that is no topic (say "## Notes") is read as a topic
  and comes back as "no result written" until it is deleted or moved below "Tasks".
- A house-infra `messages: quiet` is a default the room may change unless `room_settings` is off;
  without `organizers` everyone in the room may change the settings.
- A rollback to an image from before drops what waits in the state (`open_topics`, the link
  message, the new close steps): those topics stay on their closed page and are not carried on;
  rolling forward again duplicates nothing (`has_heading`, `has_box`).
- After the update, cancelled meetings the old image kept open get "(cancelled)" in their
  title; their pages (old template, no "Topics") carry nothing.
- Matrix keeps a message's earlier versions: an edited link message still shows the old text
  in its history.

## 10. Design review (5 independent reviews) and what changed

| Review | Finding | Change |
|---|---|---|
| UX | `link` mode cuts out people without VPN, and says nothing of a cancellation | `quiet` edits its one message on a move/cancellation; `all` stays the default; §8 |
| UX | `Result:` fails silently (a deleted line, a decision before it) | decisions anywhere; no result line = carried; two = "to check"; the box shows how each topic was read |
| UX | template too long, an example topic would be carried forever | slim template, no example topic, help in a notice the parser skips |
| UX | `messages = link` clashes with `!minutes link` | `quiet` / `all` |
| Simplification | `Type:`, reminder agenda, `carry_over` switch, German aliases for everything: unused or risky | dropped; the goal is template text only; only "Result/Ergebnis", "Topics/Themen", "Tasks/Aufgaben" are read |
| Simplification | revert 8a0f700 as a whole | done (working tree), kept only what the new design needs |
| Simplification | drop room settings | kept, smaller (3): the owner asked for settings in the room |
| Architecture | new keys in `[defaults]`/`[[plenum]]` break a rollback (`deny_unknown_fields`) | top-level `[pages]` |
| Architecture | opening depends on a successful close | time-based rule |
| Architecture | carried items duplicated; cancelled meetings unspecified | carry at close or at prepare, cleared when placed, heading check; "(cancelled)" via the title, carried and closed after its time |
| Architecture | result box not in the PDF, a retry writes it twice | its own step before the PDF, confirmed at a later tick, skipped by the extraction |
| Architecture | `announce_again` would bypass the gate | every send tagged; `quiet` edits instead |
| Privacy | topics from a confidential meeting reach a full meeting's room PDF | the receiving meeting takes the stricter publication |
| Privacy | reminder agenda puts wiki text into the room | no agenda in the room |
| Privacy | `publication` must stay out of room settings | stays out |
| Outline | patch with a missing text is a 400, the bot only knew 404 | fixed |
| Outline | Shift-Enter `\n` breaks the line parser (also today's extraction) | lines split |
| Outline | marker after the last `Result:` read as a result; box lines would join | markers skipped; the box as a list |
| Outline | API edit can be lost in a race with a browser save | written, confirmed at a later tick, idempotent |

## 11. Implementation review (4 independent reviews, a test against a real Outline 1.12.0)

| Review | Finding | Change |
|---|---|---|
| Real Outline | a carried topic's heading lost its `_` escapes (Outline drops them) and became italics: never found on the next page, so written again at every tick | the heading written the way `!top` writes one, a lone `_` re-escaped, `has_heading` compares without emphasis or escapes; both real-Outline tests pass |
| Correctness | blocks always written as `###`: on a page with `##` topics a carried or chat topic became part of the topic above and was lost | written at the page's topics level (`topics_level`, `at_level`) |
| Correctness | a topic named "Tasks" ended the topics; `has_heading` matched headings outside them | the end only at the level of "Topics" or above; `has_heading` compares the page's topics |
| Correctness | Shift-Enter cut decisions and tasks short (also on old pages) | the continuation joined back on in `extract` |
| Correctness | a `:::` line in chat text hid the following topics | chat text cannot start a notice; notices only Outline's four kinds with a closing line |
| Correctness | a cancelled page passed on decisions nobody took; a topic could land on two pages; a failed setting stayed in settings.json | carried decision lines become proposals; `prepare` takes only the topics it is the next page for; saved again after the undo |
| Pages, UX | a topic typed without "Result:" always came back, unexplained; one wrong heading level dropped topics; "Result: Decision: …" was no decision | the help says "Result:" is the last line; the carried copy says why it came; the level rule above; a decision right after "Result:" counts |
| Pages, UX | box without topic names; markers easily deleted; texts said "item"/"minutes"; the help as one long paragraph | "✅ <topic>: <decision>"; markers say "please leave this line"; texts say "topic"/"page"; the help as short points |
| Pages, UX | docs differed from the code (template, box, cancelled pages, setting notice, house-infra titles, markers) | aligned (this file, README, house-infra docs/PLENUM.md, BOTS.md, matrix.yml, app.js) |
| Quiet mode | the gate holds on every path; a house-infra `quiet` can be undone in the room; an older image's link message cannot be edited; answers promised a summary in a quiet plenum | documented as a default (`room_settings: false` fixes it); a move then posts the link anew; the answers say what a quiet plenum gets |
| Simplification | the PDF printed the new markers and the help; the last fallback put chat topics under a heading that hid the real ones; three copies of the "Topics" lookup; German words the design had dropped | the PDF leaves out markers and the help; the fallback appends the block (under "## Topics" only on a page without topics); one lookup; only Result/Ergebnis, Decision/Beschluss, Topics/Themen, Tasks/Aufgaben stay |
| Simplification | settings rate limits, settings.json, the `Notice` tags | kept: the limits keep a room from flipping a plenum back and forth, settings.json survives a rollback, the tags let the tests check every message's kind |

## 12. Next page after the previous plenum, pins, publication link (2026-11-07)

Owner: every plenum `publication: link` (house-infra `plenum.defaults`), `messages: all`, no
organizers; the next page from the day after the previous plenum; the current page easy to find
in Outline; the wiki stays VPN-only. Built: `page_after_days` (§4.1), pins (§6.1).

| Review | Finding | Change |
|---|---|---|
| Design | `prepare_days 60` alone gives "right after" too | kept `page_after` for the delay the owner asked for; one rule decides (with it `page_days` only for a first meeting), texts say which |
| Design | a cancelled meeting cannot count as "the one before" (one without a page leaves the state) | the same previous meeting as before (cancelled and skipped left out) |
| Design | a series edit in Nextcloud gives new IDs: the long-open page cancelled, a second one opened | re-keyed to the new occurrence on its day |
| Design | two meetings at one time both got the carried topics | ordered by ID (prepare, carrying topics and tasks); an event entered twice is one meeting |
| Design | the collection description's "Overview" tab is the default only until someone picks another | pins instead (shown on every tab); the key gets `pins.*` |
| Design | `!minutes done` promised "a summary here" in a `link` meeting | the answer follows the publication |
| Real Outline 1.12.0 | `pins.list`/`create`/`delete` as expected (create idempotent, delete of a missing pin 404, no scope 403); a member who is no collection admin reads pins but gets 403 on create and delete, which failed the whole round | 403 on unpin counts as "not allowed"; test `pins_in_a_real_outline` |
| Code | people's pins of closed pages were removed | only what the bot pinned is unpinned |
| Code | a deleted open page stopped the other pins | each page tried on its own |
| Code | the dedup could drop a per-meeting publication (and cancel a meeting still in the calendar) | an occurrence with state of its own is never dropped |
| Code | the re-key moved a meeting already under way | only meetings still to come |
| Code | two tests could not fail | assert the move message; count every pins call |

