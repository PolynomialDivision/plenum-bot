# Topics: bringing things to the plenum, prepared

Status: design 2 (2026-11-07), after an independent review from four perspectives (UX and
community process, architecture, security and privacy, Matrix clients and simplicity), and
implemented (branch `topic-preparation`; the README's "Topics" says what the bot does).
Section 10 lists what the design review found and what changed, section 11 the review of the
implementation; section 3 the alternatives that were dropped.

## 1. The problem

About 34 people live in the house; few come to the plenum. The bot already makes the
minutes (Outline), puts `!top <topic>` from the chat on the agenda, files decisions and
carries open tasks on. What is missing is everything around a topic:

- a topic is one line on the agenda: nobody knows what it is about or whether its author
  wants to talk about it or have it decided, nor whether there is a proposal;
- there is no visible place to talk about a topic beforehand, so everything starts from
  zero in the meeting, which makes it long, and people who cannot come have no say;
- afterwards nobody hears what came of "their" topic unless they read the minutes.

People come when they can shape something and see that it has an effect. So: bringing in a
topic takes one message; before the plenum everyone sees what is coming and can say
something without attending; afterwards whoever cared hears the outcome, where they talked
about it.

## 2. What exists and is reused

| Need | Already there | Use |
|---|---|---|
| Submitting | `!top <topic>`, `!top` as a reply | stays the way in; becomes a numbered topic |
| Agenda, minutes, decisions, open tasks | the Outline protocol from the template, `Decision:` lines, open checkboxes carried on | unchanged; topics get an agenda line and a section of their own |
| Dates, moves, cancellations | the plenum's Nextcloud calendar | topics follow the meetings |
| Who may take part | the rooms follow the Authentik groups (`[members]`) | room membership is the right to take part; no list of its own |
| Who moderates | `organizers` per plenum (none in production), bot admins | the same people |
| What may go into the chat | per meeting `publication` (full, link, confidential) | every topic message obeys it |
| Buttons, threads, edits | mxbot-common `reaction`, `send::in_thread`, ruma `make_replacement`; cleaning-bot, houseops-bot | the same building blocks |
| Anonymous feedback | kummerkasten-bot (private chat, survey-only in production) | not duplicated: plenum topics are named and public in the plenum's room |
| Task tracking | houseops-bot (never deployed) | not duplicated: tasks stay checklist lines in the minutes |
| Websites with forward auth | cleaning-bot, schnaeppchenjaeger-bot | the pattern for a later page (E1) |

## 3. Approaches considered

**A. Chat and threads.** The bot answers `!top` with a topic message of its own in the room;
its thread is the discussion; 👍 under it is interest; the announcement carries the agenda;
the outcome goes into the thread after the plenum.

**B. Wiki first.** Each topic an Outline document (what, goal, proposal), discussion in
Outline's comments, the bot lists and links them.

**C. A web app** (`plenum.<domain>`, forward auth): a form, a topic board with interest
buttons, agenda, results; Matrix only for notifications.

**D. A guided private chat** (as kummerkasten-bot): the bot asks step by step, then posts.

| | A chat + threads | B wiki | C web app | D guided chat |
|---|---|---|---|---|
| ease of use | ★★★ one message | ★★ an editor on the phone | ★★★ a form | ★★ many steps |
| low entry barrier | ★★★ what people do in the room anyway | ★ the wiki only at home or over VPN | ★★ another address, a login | ★★ a private chat to find |
| motivation | ★★★ visible in the room, replies and 👍 are social | ★ comments nobody sees | ★★ a board, the talk elsewhere | ★ private until done |
| Matrix integration | ★★★ native | ★ none | ★★ notifications | ★★ |
| per-plenum config | ★★★ | ★★ | ★★★ | ★★ |
| maintainability | ★★★ inside the bot | ★★ parses free documents | ★ a second UI, CSRF, i18n | ★ a dialog engine |
| security | ★★★ the room's members, nothing new | ★★★ | ★★ a new exposed service | ★★★ |
| effort | medium | medium | high | high |
| extensibility | ★★★ a page can come on top | ★★ | ★★★ | ★★ |

**Chosen: A.** B fails on reach (the wiki is internal; on a phone awkward; comments reach
nobody). C splits the conversation between two places and is the largest new attack
surface; it can come later as a read-mostly page (E1). D hides the topic from the room
until it is "finished", the opposite of what invites people in.

## 4. Principles

1. **One message is enough.** `!top <title>` is a complete topic; everything else is
   optional.
2. **The plenum decides.** The bot shows what is coming and what came of it; it never votes,
   never ranks topics by popularity, never closes a topic by its own judgement.
3. **The chat messages stand on their own.** The wiki is internal (home or VPN): a message
   that only links there helps few people on a phone.
4. **No new administration.** Who takes part is who is in the room; who moderates is
   `organizers` and the bot admins. No list of people of its own.
5. **Rights are checked on the server, for every action, never stored**: the author (as a
   person, all their accounts), the organisers, the admins. Others' events never change a
   topic.
6. **The live events are a hint, the server is the truth.** Reactions are recounted from the
   homeserver before each step; a missed event changes nothing that matters.
7. **The bot never rewrites what people write**: it inserts at markers it owns, as today.
8. **Little data.** Ordinary thread messages are never copied or stored; who reacted is
   never stored.

## 5. How it works for residents

### Bringing in a topic

```
Anna:  !top Washing machine in the cellar is broken
```
The bot answers in the room, as it does today, now with the topic itself (a reply to Anna's
message, mentioning her):
```
📌 #12 Washing machine in the cellar is broken
from Anna · to talk about · for the plenum on Sun 16 Nov, 19:00

👍 if it matters to you: you hear what came of it. Discuss it: reply in this message's
thread. Anna: tap 🤝 to make it a topic to decide.
```
That message is the topic from now on: its thread is the discussion, 👍 under it is
interest, the bot keeps it up to date (kind, proposal, date, and finally the outcome).
The bot puts 👍 and 🤝 under it as buttons.

- **To decide or to talk.** A topic is "to talk about" unless its author (or an organizer)
  taps 🤝, or it starts so: `!top decide: Buy a new washing machine` (also `entscheiden:`).
- **More at once.** Further lines of the message are the description; a line starting with
  `Proposal:` or `Vorschlag:` is the proposal. The author (only they) can add or replace the
  proposal later with such a line in the topic's thread, or by editing the `!top` message.
- **Someone else's message.** A reply with `!top` to it makes its first line the topic,
  brought in by whoever wrote `!top` ("Ben (Anna's message)"); nothing more of it is copied.
  Anna may withdraw it like Ben, and deleting her message withdraws it.

### Before the plenum

- **The thread** is an ordinary thread. The bot reads none of it except the author's
  `Proposal:` lines. Can't come? Write there; the topic's section in the minutes links to it,
  and the facilitation sees it.
- **The agenda** comes with the announcement (`prepare_days` before, as today) and the
  announcement stays current: the bot edits it as topics come, go or change. Decisions first,
  then the talks, each in the order they came:
  ```
  📋 House plenum on Sunday, 16 November 2026 at 19:00

  To decide:
  • #12 Washing machine in the cellar (Anna): proposal: buy a new one, up to 600 €
  • #15 Quiet hours (Chris) · short notice (came after the agenda)
  To talk about:
  • #11 Garden day in spring (Dana)
  • #13 Heating in the attic (Eli) · again, not reached on 2 Nov

  Agenda and minutes: <link>
  …
  ```
  The agenda comes right under the title (where a phone shows it). Each `#12` links to the
  topic's message. A topic to decide that comes after the agenda went out is marked "short
  notice": people who read the chat once a week had three days for the others. What the
  plenum makes of that is its own rule; the bot only shows it.
- **In the minutes**, made at the same time, each topic has an agenda line and a section of
  its own under "Minutes" that the bot writes:
  ```
  ### #12 Washing machine in the cellar
  > Anna, to decide. The old one leaks since October.
  > Proposal: buy a new one, up to 600 €
  ↳ Discussion in the chat: #12 (link)
  ```
  The facilitation writes below it what was said, `Decision:` lines and tasks, as always.

### After the plenum

When the minutes are closed (as today), each topic's section is read:

| Its section | The topic's thread gets | The topic |
|---|---|---|
| has `Decision:` lines | "✅ Decided on 16 Nov: …" with the decisions and the section's tasks | done |
| has notes, no decision | "💬 Talked about on 16 Nov" | done |
| only what the bot wrote | "⏭️ Not reached on 16 Nov: it is on the agenda of …" (no mentions); without `carry_over`: "`!top #12 again` puts it on the next agenda" | carried over (without `carry_over`: unclear) |
| gone (heading deleted) | nothing | unclear: "on the agenda of 16 Nov"; `!top #12 again` brings it back |

When no topic's section has anything written in it but the minutes have decisions elsewhere
(written the old way, under headings of their own), nobody knows which topic was reached:
every topic of that meeting is unclear, nobody is told "not reached" and nothing is carried
on. The wiki's overview, the template and the example minutes say to write under the topics'
sections.

The first two mention the author and everyone who 👍'd and is still in the room (at most
20), and the topic's message is edited to show the outcome. What the room gets follows the
meeting's `publication`: `link` gives the link to the minutes instead of the decisions,
`confidential` only "dealt with on 16 Nov" (in the thread and on the topic's message).

### Changing one's mind

| What | How | Who |
|---|---|---|
| list the topics | `!top` alone | everyone |
| to decide / to talk | 🤝 on the topic's message, or `!top #12 decide`, `!top #12 talk` | author, organizers |
| new text | edit the `!top` message | its sender |
| the proposal | a `Proposal:` line in the `!top` or in the thread | the author |
| withdraw | delete the `!top` message (or the topic's message), or `!top #12 withdraw` | author, organizers |
| not this time | `!top #12 later` (the meeting after the next) | author, organizers |
| dealt with | `!top #12 done` | author, organizers |
| back on the agenda | `!top #12 again` | author, organizers |

The actions also take German words (`zurückziehen`, `später`, `erledigt`, `nochmal`,
`entscheiden`, `besprechen`), a number without `#` and a trailing dot (as a phone writes
them). The author is the person: any account the directory knows as theirs (while its last
read is fresh and nobody else claims that account), else exactly the account that wrote
`!top`; for a reply-`!top` also the sender of the message it brought in. Organizers are the
plenum's `organizers` and the bot admins. Unlike `!minutes done`, a plenum without
`organizers` does not let everyone act on others' topics: the author's topic is theirs.

### What else happens

- **Cancelled** in the calendar: its topics go to the next meeting (the cancellation notice
  says so; each topic message shows the new date). **Moved**: the topics stay with it.
- **No meeting in the calendar**: the topic is "for the next plenum", placed when one comes.
- **Offboarding**: topics stay (they belong to the house); only people still in the room are
  mentioned.
- **Restarts**: every message has a transaction ID saved before sending and its event ID
  kept; the reactions are counted again from the server; nothing is sent twice or lost.

## 6. Settings

| Layer | Where | Who | Takes effect |
|---|---|---|---|
| technical | house-infra `matrix.yml` → config.toml: plenums, rooms, groups, access, calendars, organisers, `[topics]` (enabled, and whether rooms may change settings) | IT admins | git, `make update-bots` |
| house standards | the same file: `plenum.defaults` (agenda days, reminder) and `[topics]` (carry over, tell the interested) | IT admins, on the house's wish | git, deploy |
| the plenum's own | in its room: `!plenum settings`, `!plenum set <name> <value>`, `!plenum reset <name>` | its organisers and the admins; without `organizers`: everyone in the room | at once, said in the room, listed in the wiki overview |

The plenum's own settings, each shown by `!plenum settings` in plain words with its value,
where the value comes from and the command to change it:

| Name | Default | Meaning |
|---|---|---|
| `topics` | on | numbered topics, the agenda in the announcement, outcomes; off: `!top` as before |
| `agenda_days` | `prepare_days` (3) | how many days before the agenda and the minutes come (1–14) |
| `reminder` | `remind_minutes` (30) | minutes before the start (0 = none, at most 1440) |
| `carry_over` | on | not reached: onto the next agenda |
| `tell_interested` | on | the outcome mentions who 👍'd |

Order: code default < house-infra `[defaults]` and `[topics]` < the plenum's entry < the
room's value. `topics = false` in house-infra cannot be switched on in the room. A change
names who made it and the old and new value in the room; each person may change at most 10
values a day, and a plenum's settings change at most 30 times a day (the bot admins are not
limited). Everything that decides who may read or do what (rooms, groups, access,
publication, organizers) is not a room setting.

## 7. Data model (store/topics.json)

A file of its own, saved under the same lock as state.json: an older image (a rollback)
neither reads nor overwrites it.

```rust
pub struct Book { pub plenums: BTreeMap<String, Area> }       // by plenum key
pub struct Area {
    pub last: u32,                                            // the last number (never reused)
    pub topics: BTreeMap<u32, Topic>,
    pub settings: BTreeMap<String, String>,                  // the room's values, raw
    pub changes: Vec<Change>,                                 // who changed what (30 days)
    pub announcements: BTreeMap<String, Posted>,             // per meeting: event, text, edits
    pub migrated: bool,                                       // pending_tops moved
}
pub struct Topic {
    pub number: u32, pub title: String,                       // ≤ 200 chars
    pub details: String, pub proposal: Option<String>,        // ≤ 1500 / 1000 chars
    pub proposal_event: Option<String>, pub proposal_by: Option<String>,
    pub decide: bool,                                         // by `decide:` or a command
    pub tap_decide: bool,                                     // the author's 🤝 (recounted)
    pub by: String, pub by_user: Option<String>,              // name as shown, account
    pub from: Option<String>, pub from_user: Option<String>,  // a reply-!top's message:
    pub from_event: Option<String>,                           //   its sender and event
    pub at: DateTime<Utc>,
    pub source: Option<String>,                               // the !top message (edits, deletion)
    pub card: Step, pub card_event: Option<String>,           // the topic's message
    pub card_text: Option<String>, pub card_edits: u32,       // what it shows; edits sent
    pub buttons: bool,                                        // 👍 and 🤝 are under it
    pub not_after: Option<MeetingKey>,                        // `later`: after this meeting
    pub placed: Option<String>,                               // the meeting whose minutes have it
    pub short_notice: bool,
    pub status: Status,                                       // Open | Done | Withdrawn | Unclear
    pub carried: u32, pub history: Vec<Outcome>,              // per meeting: result, link,
                                                              //   confidential, told
    pub outcome: Step, pub outcome_attempts: u32,
    pub dirty: bool, pub counted_at: Option<DateTime<Utc>>,   // reactions to count again
    pub deleted: bool, pub closed_at: Option<DateTime<Utc>>,
}
```

- **Rooms and events.** A topic is found by (room, event): an event from another room never
  touches it. Edits count only from the original's sender, read from `m.new_content`.
- **Edits.** Each edit of a topic's message or an announcement gets a new transaction ID
  (`card_edits`, `Posted.edits`, saved before it is sent): an edit back to an earlier text is
  not dropped by the homeserver as a resend.
- **Retention.** Description and proposal are cleared 30 days after a topic is done or
  withdrawn; the topic is dropped after 180 days; setting changes after 30 days. This runs for
  every plenum, also with topics switched off. A deleted `!top` message (or the message a
  reply-`!top` brought in) clears them at once and the title becomes "(deleted)" (the minutes
  keep what was written into them). Who 👍'd is never stored: it is asked of the server when
  needed.
- **Old state.** `pending_tops` become topics without a topic message (placed like today's
  items); they are cleared from state.json once moved.
- **Limits.** 10 topics per person and day, 15 open topics per person and 60 per plenum.

## 8. Architecture

- **plenum-bot only.** `src/topics.rs`: the model, parsing of `!top` text and proposal lines,
  the texts (topic message, agenda, protocol lines, outcomes), reading a topic's section from
  the minutes; pure functions with unit tests. `src/engine/topic_flow.rs`: the engine's part
  (a second `impl Engine`, a child module of the engine): commands, events, the tick steps;
  its user-flow tests in `src/engine/topic_flow/tests.rs`. `src/plenum_settings.rs`: the
  room's settings (names, parsing, bounds, plain words). No change to mxbot-common.
- **Matrix.** The `Chat` trait gets `post` (send and return the event ID), `edit`,
  `react` and `reactions` (the annotations of an event, paged from `/relations`, decrypted).
  main.rs passes edits, thread replies, reactions and redactions in plenum rooms to the
  engine; their handlers only note them (no lock, no network: the matrix-sdk waits for them
  in the sync), and the tick handles and saves them before it sends anything because of
  them. Thread messages are noted only when they start with a proposal label. Reactions are
  read oldest first (at most 1000; a cut-off list never takes a 🤝 away). Replayed history from an empty store is ignored
  (`events::is_replayed`); `!top` is also taken after downtime (the 15-minute rule stays for
  `!minutes`), and a `!top` event already seen is ignored.
- **The protocol.** At prepare the bot builds the text locally (as today): the agenda line
  per topic at the agenda marker, the section per topic at a second marker,
  `➕ Sections for the topics go here (plenum-bot).`, which every protocol the bot makes gets
  (the template's place, else above the next `##` after "Minutes", else the end). Later
  topics go in at both markers (Outline `patch` of the bot's own marker, never of text with
  user input). Every line from people is flattened as `top_line` does and quoted (`>`): no
  heading, checkbox, `Decision:`, `Task:` or marker (`➕` becomes `+`) can come from it. A topic is found in the minutes
  by its heading `### #12 …` only, never by its title.
- **Steps.** `outcomes` is its own step after the summary, one per topic, each with its
  transaction ID and at most 3 attempts, so one failing thread holds up nothing else.
- **house-infra.** `[topics]` in the template only when matrix.yml sets `plenum.topics`
  (top-level table: an older image ignores it); validate.py checks it; docs/PLENUM.md and
  docs/BOTS.md describe it for residents; the start page's bot text gets one line. No new
  service, secret, network or DNS. Roll out the bot first, then the config.

## 9. Extensions, deliberately not now

- **E1 a topic page** for people who rarely use Matrix (cleaning-bot's forward-auth pattern),
  read-only first.
- **E2 📌 under any message** makes it a topic (no typing on the phone).
- **E3 the agenda in the calendar event**, for people who only look there.
- **E4 from the Kummerkasten**: an organiser turns an anonymous submission into a topic.
- **E5 a consent list**: decision topics with a proposal and no "✋ I have concerns" by the
  agenda are confirmed together at the start.
- **E6 roles**: 🎙️ facilitation and ✍️ minutes volunteered under the announcement.
- **E7 the bot's texts in German** (the bot speaks English since 2026-10-22; a catalogue for
  all its texts is a change of its own).

## 10. Review (2026-11-07): what was found, what changed

| Finding | Decision |
|---|---|
| A topic card *inside* the thread of the user's message is invisible on phones, and a user's message cannot be edited to show the outcome (Matrix, UX) | the bot's own topic message in the room is the root, the 👍 target and the record; it replaces today's one-line answer, so the room gets no more messages than now |
| "Events that arrive while the bot is down come with the next sync" is false: limited timelines, late keys, the 15-minute rule (Matrix, architecture) | reactions are recounted from `/relations` before the agenda and the outcomes and when a topic changed; handlers only mark; `!top` is taken after downtime and deduplicated by its event |
| Handlers awaiting the state lock (held across Outline calls) stall the sync (architecture) | handlers never do network calls; the tick sends |
| Relations, redactions and edits can point at events in other rooms; edits from another sender (security H1, H2) | lookup by (room, event); edits only from the same sender; redactions of a `!top` count from its author, organisers, admins |
| Without organisers everyone could withdraw, re-kind or close others' topics (security H3) | topic actions: author, organisers, admins; settings stay open to the room (announced, bounded, rate-limited) |
| Anyone's `Proposal:` would replace the author's; `Summary:` by anyone frames the debate (UX, security H4) | the proposal is the author's; `Alternative:`, `Question:`, `Summary:` dropped |
| English labels are a mini-language for German speakers (UX) | one label, `Proposal:`/`Vorschlag:`; kinds by a tap or `decide:`/`entscheiden:`; German texts as E7 |
| Outcomes by people's `### #12` headings or titles are unreliable; "not reached" by a missing section would mis-carry topics (all) | the bot writes the sections; "not reached" only when its section is untouched; a missing heading carries nothing |
| Patching with `findText` of a line containing user text fails on Outline's escaping (architecture) | patches only of the bot's own markers |
| Multi-line user text in the minutes can forge decisions and tasks (security M3, architecture) | every line flattened and quoted; tests |
| Rollback of the image would drop topics and reset numbers (architecture) | `store/topics.json` of its own |
| `[defaults.topics]` or per-plenum keys make an older image refuse its config (architecture) | a top-level `[topics]` table, rendered only when set |
| The wiki is internal; link-only messages help few (UX) | the agenda and the outcomes carry the text; `link` meetings: the user's choice, kept |
| A third "briefing" message, time boxes, ordering by 👍, counts in the agenda: noise, popularity contest (UX, Matrix) | the agenda goes into the announcement, edited in place; no counts; no ordering by interest |
| Eleven room settings that everyone may change (UX, architecture) | five low-risk settings; the rest technical or not at all |
| `participants`, `interest` user IDs, thread texts in a backed-up state (security M6) | not stored; texts cleared 30 days after a topic ends |
| Decision topics added the evening before (UX) | "short notice" after the agenda went out, shown in the agenda and on the topic |
| The "it's read out" promise for people who can't come has nobody to keep it (UX) | dropped; the thread is linked from the topic's section, nothing more is promised |
| Display names are user-controlled; markdown injection into bot messages, `@room` (security M3, M4) | names from the directory when known, capped and stripped; user text escaped; explicit mentions only |
| Outcomes in `close` would hold up the summary (architecture) | a separate step per topic, 3 attempts |
| Per-thread cancellation notices: noise (architecture) | one sentence in the cancellation notice; the topic messages show the new date |

## 11. Review of the implementation (2026-11-07)

Three independent reviews (correctness and architecture, security and privacy, UX and
documentation), with scratch tests proving their findings. What they found and what changed:

| Finding | Change |
|---|---|
| A title or description containing the bot's marker text made every later topic's section land inside it (Outline patches the first match): decisions counted for the wrong topic | `➕` from people becomes `+` (topics and the old `!top` items); test |
| Edit transaction IDs derived from the text: an edit back to an earlier text (🤝 on, off, on; withdraw, again, withdraw) was dropped by the homeserver as a resend, the message stayed wrong | a counter per message, saved before each edit; the fake homeserver drops repeated IDs; test |
| Inbound events changed the state but were saved only at the end of the tick, after the edits they caused | saved right after they are handled |
| The facilitation was still told to write its own `###` headings and decisions under "Decisions and tasks": topics would be "not reached" wrongly | overview, template and example minutes say to write under the topics' sections; minutes with no section written but decisions elsewhere: every topic unclear, nobody told, nothing carried; test |
| A confidential meeting's outcome showed "Decided" on the topic's message | "Dealt with" only; test |
| The agenda came below the announcement's other text; "write it into the document" makes no topic | the agenda right under the title; that phrase dropped with topics; test |
| The card promised "you hear what came of it" with `tell_interested` off | said only when on |
| Retention stopped when the room switched topics off; changes kept by count only | every area pruned on every tick; changes after 30 days; test |
| The settings limit could be passed with several accounts | also 30 changes per plenum and day |
| A reaction flood could push the author's 🤝 out of the count; any redaction made every topic be counted again | read oldest first, a cut-off list never takes a 🤝 away; unknown redactions wait for the hourly count |
| A reply-`!top`'s quoted person had no say | they may withdraw it; deleting their message withdraws it and clears it ("(deleted)"); test |
| Proposals from the thread were taken from organizers too (design: the author's) | the author's only |
| One person could fill a plenum's 60 open topics | 15 open per person |
| Crash between topics.json and state.json: pending items moved twice | moved items are recognised; test |
| Buttons lost when the reaction failed after the card was saved | `buttons` flag, retried at the next tick |
| A topic arriving while two meetings are prepared went into the later one | an earlier open meeting takes it first |
| A moved meeting's old announcement was edited while the new one was due | only announced meetings' announcements follow the agenda |
| Refusals named "organisers" nobody has; jargon ("house-infra", "the house's"); typos (`!top 12 Later.`) made junk topics | plain refusals naming who may act; "default", "set centrally by the admins"; tolerant actions; a hint for `!plenum settings <name> <value>`; help text rewritten |
| The failing-thread test never reached the give-up | a fake that fails only threads; the test checks three attempts and the summary |

Left as they are (documented): edits of a topic after its minutes were made do not change
the minutes; a moderator who is neither author, organizer nor admin cannot withdraw a topic
by deleting its message; names are fixed when a topic is made; texts stay in the edit history
of the bot's messages in Matrix.
