//! Topics end to end, as people use them: bringing one in, the agenda, the minutes, what came
//! of it, and the changes in between (docs/TOPICS.md).

use super::*;
use crate::{
    commands::{parse_for, Command},
    config::{Access, TopicRules},
    engine::fakes::*,
};
use chrono::{Datelike, TimeZone};
use mxbot_common::membership::Member;

const ROOM: &str = "!house:x";
const BOT: &str = "@plenum-bot:x";

fn plenum() -> Plenum {
    Plenum {
        key: "house".into(),
        name: "House plenum".into(),
        room: ROOM.try_into().unwrap(),
        collection: "House plenum".into(),
        calendar: "plenum-house".into(),
        prepare_days: 3,
        remind_minutes: 30,
        duration_minutes: 120,
        close_after_hours: 12,
        organizers: HashSet::new(),
        group: None,
        require_group: None,
        access: Access::House,
        publication: Publication::Full,
        topics: TopicRules::default(),
    }
}

struct T {
    e: Engine<FakeWiki, FakeChat>,
    dir: tempfile::TempDir,
}

fn first_sundays() -> Vec<Occurrence> {
    (0..9)
        .map(|i| {
            let month = (9 + i) % 12 + 1;
            let year = if month >= 10 { 2026 } else { 2027 };
            let first = NaiveDate::from_ymd_opt(year, month, 1).unwrap();
            let day =
                first + Duration::days(i64::from((7 - first.weekday().num_days_from_sunday()) % 7));
            let start: MeetingKey = format!("{day} 19:00").parse().unwrap();
            Occurrence {
                id: format!("house-series#{day}"),
                start,
                end: Some(start.0 + Duration::hours(2)),
            }
        })
        .collect()
}

fn engine(p: Plenum, dir: &std::path::Path, state: State) -> Engine<FakeWiki, FakeChat> {
    let mut e = Engine::new(
        vec![p],
        "Europe/Berlin".parse().unwrap(),
        FakeWiki::default(),
        FakeChat::default(),
        PdfConfig {
            enabled: false,
            font_dir: String::new(),
            post_to_room: false,
        },
        HashSet::from(["@admin:x".try_into().unwrap()]),
        state,
        dir.join("state.json"),
    );
    e.me = Some(BOT.try_into().unwrap());
    e
}

fn setup_with(p: Plenum) -> T {
    let dir = tempfile::tempdir().unwrap();
    let e = engine(p, dir.path(), State::default());
    {
        let mut st = e.state.try_lock().unwrap();
        let ps = st.plenums.entry("house".into()).or_default();
        ps.occurrences = first_sundays();
        ps.calendar_read = Some(at(2026, 10, 1, 0, 0));
    }
    for who in ["@anna:x", "@ben:x", "@chris:x"] {
        e.chat.members.lock().unwrap().push((
            ROOM.into(),
            Member {
                id: who.try_into().unwrap(),
                admin: false,
                joined: true,
            },
        ));
    }
    T { e, dir }
}

fn setup() -> T {
    setup_with(plenum())
}

/// Berlin local time.
fn at(y: i32, m: u32, d: u32, h: u32, min: u32) -> DateTime<Utc> {
    let tz: Tz = "Europe/Berlin".parse().unwrap();
    tz.with_ymd_and_hms(y, m, d, h, min, 0)
        .unwrap()
        .with_timezone(&Utc)
}

fn person(id: &str, name: &str) -> Sender {
    Sender {
        id: id.try_into().unwrap(),
        name: name.into(),
    }
}

fn anna() -> Sender {
    person("@anna:x", "Anna")
}

fn ben() -> Sender {
    person("@ben:x", "Ben")
}

fn room() -> OwnedRoomId {
    ROOM.try_into().unwrap()
}

/// `body` from `who` as the message `event` in the house room.
async fn say(
    e: &Engine<FakeWiki, FakeChat>,
    who: &Sender,
    body: &str,
    event: &str,
    now: DateTime<Utc>,
) -> String {
    let cmd = parse_for(body, &["house"]).expect("a command");
    let origin = Origin {
        event: Some(event.into()),
        ..Default::default()
    };
    e.command_from(&room(), who, cmd, now, Scope::Room, &origin)
        .await
}

fn posts(e: &Engine<FakeWiki, FakeChat>) -> Vec<Post> {
    e.chat.posts.lock().unwrap().clone()
}

fn topic(e: &Engine<FakeWiki, FakeChat>, n: u32) -> Topic {
    e.state.try_lock().unwrap().book.plenums["house"].topics[&n].clone()
}

fn doc(e: &Engine<FakeWiki, FakeChat>, title: &str) -> String {
    e.wiki.by_title(title).expect(title).text
}

fn edit_doc(e: &Engine<FakeWiki, FakeChat>, title: &str, f: impl FnOnce(&mut String)) {
    let mut docs = e.wiki.docs.lock().unwrap();
    let (d, _) = docs.iter_mut().find(|(d, _)| d.title == title).unwrap();
    f(&mut d.text);
}

fn last_edit_of(e: &Engine<FakeWiki, FakeChat>, event: &str) -> Option<String> {
    e.chat
        .edits
        .lock()
        .unwrap()
        .iter()
        .rev()
        .find(|(_, ev, _)| ev == event)
        .map(|(_, _, t)| t.clone())
}

fn threads(e: &Engine<FakeWiki, FakeChat>) -> Vec<Post> {
    posts(e)
        .into_iter()
        .filter(|p| p.place.starts_with("thread:"))
        .collect()
}

#[tokio::test]
async fn a_topic_from_its_message_to_what_came_of_it() {
    let t = setup();
    let e = &t.e;

    // Anna brings in a topic to decide, with a description and a proposal.
    let answer = say(
        e,
        &anna(),
        "!top decide: Washing machine in the cellar\nThe old one leaks.\nProposal: buy a new one, up to 600 €",
        "$anna1",
        at(2026, 10, 20, 12, 0),
    )
    .await;
    assert_eq!(answer, "", "the topic's message is the answer");
    let card = &posts(e)[0];
    assert_eq!(
        (card.room.as_str(), card.place.as_str()),
        (ROOM, "reply:$anna1")
    );
    assert_eq!(card.users, vec!["@anna:x"]);
    assert!(
        card.text.contains("#1 Washing machine in the cellar"),
        "{}",
        card.text
    );
    assert!(card.text.contains("to decide"), "{}", card.text);
    assert!(card.text.contains("Sun 1 Nov, 19:00"), "{}", card.text);
    assert!(
        card.text.contains("Proposal: buy a new one"),
        "{}",
        card.text
    );
    let buttons: Vec<String> = e
        .chat
        .reacted
        .lock()
        .unwrap()
        .iter()
        .map(|(_, ev, k)| format!("{ev} {k}"))
        .collect();
    assert_eq!(buttons, vec!["$ev1 👍", "$ev1 🤝"]);
    let card_event = topic(e, 1).card_event.unwrap();

    // `!top` alone lists it.
    let list = say(e, &ben(), "!top", "$ben0", at(2026, 10, 21, 9, 0)).await;
    assert!(
        list.contains(") Washing machine in the cellar (Anna) · to decide"),
        "{list}"
    );

    // Three days before: the minutes have its agenda line and its section; the announcement
    // lists it.
    e.tick(at(2026, 10, 29, 19, 0)).await;
    let text = doc(e, "House plenum 2026-11-01");
    assert!(
        text.contains("* #1 Washing machine in the cellar (Anna) · to decide"),
        "{text}"
    );
    assert!(
        text.contains("### #1 Washing machine in the cellar"),
        "{text}"
    );
    assert!(
        text.contains("> Proposal: buy a new one, up to 600 €"),
        "{text}"
    );
    assert!(
        text.contains(&format!(
            "↳ Discussion in the chat: [#1](https://matrix.to/#/{ROOM}/{card_event}?via=x)"
        )),
        "{text}"
    );
    let announcement = posts(e)
        .into_iter()
        .find(|p| p.text.contains("📋"))
        .unwrap();
    assert!(
        announcement.text.contains("**To decide:**"),
        "{}",
        announcement.text
    );
    assert!(
        announcement
            .text
            .contains("Washing machine in the cellar (Anna): proposal: buy a new one"),
        "{}",
        announcement.text
    );
    assert_eq!(
        topic(e, 1).placed.as_deref(),
        Some("house-series#2026-11-01")
    );
    assert!(!topic(e, 1).short_notice);
    // the wiki's overview lists it for those who read rather than chat
    let overview = doc(e, "House plenum: Overview");
    assert!(
        overview
            .contains("## Topics waiting\n\n* #1 Washing machine in the cellar (Anna) · to decide"),
        "{overview}"
    );

    // Ben brings in one more after the agenda went out: it goes in, the announcement follows.
    say(
        e,
        &ben(),
        "!top Garden day in spring",
        "$ben1",
        at(2026, 10, 30, 9, 0),
    )
    .await;
    e.tick(at(2026, 10, 30, 9, 1)).await;
    let text = doc(e, "House plenum 2026-11-01");
    assert!(
        text.contains("* #2 Garden day in spring (Ben) · to talk about"),
        "{text}"
    );
    assert!(text.contains("### #2 Garden day in spring"), "{text}");
    let first = text.find("### #1").unwrap();
    let second = text.find("### #2").unwrap();
    let marker = text.find(topics::SECTIONS_MARKER).unwrap();
    assert!(first < second && second < marker, "{text}");
    let edited = last_edit_of(e, &announcement.event).expect("the announcement follows");
    assert!(edited.contains("**To talk about:**\n• [#2]"), "{edited}");
    assert!(topic(e, 2).short_notice, "after the agenda went out");
    // a talk topic: "short notice" matters only for decisions
    assert!(!edited.contains("short notice"), "{edited}");
    // nothing more on the next tick
    let edits = e.chat.edits.lock().unwrap().len();
    e.tick(at(2026, 10, 30, 9, 2)).await;
    assert_eq!(e.chat.edits.lock().unwrap().len(), edits);

    // Ben 👍'd Anna's topic (the bot's own 👍 is a button, no interest).
    e.chat.reactions.lock().unwrap().insert(
        card_event.clone(),
        vec![
            ("@ben:x".into(), "👍".into()),
            (BOT.into(), "👍".into()),
            (BOT.into(), "🤝".into()),
        ],
    );

    // At the plenum: #1 is decided, #2 is not reached.
    edit_doc(e, "House plenum 2026-11-01", |t| {
        *t = t.replace(
            "### #2 Garden day in spring",
            "We talked a lot.\n\nDecision: we buy one\n\n- [ ] Ben asks for offers\n\n### #2 Garden day in spring",
        );
    });
    e.tick(at(2026, 11, 2, 9, 0)).await; // closed 12 h after the end
    let threads = threads(e);
    assert_eq!(threads.len(), 2, "{threads:?}");
    let decided = &threads[0];
    assert_eq!(decided.place, format!("thread:{card_event}"));
    assert!(
        decided.text.contains("decided at the plenum on 1 Nov"),
        "{}",
        decided.text
    );
    assert!(
        decided.text.contains("we buy one") && decided.text.contains("Ben asks for offers"),
        "{}",
        decided.text
    );
    assert_eq!(
        decided.users,
        vec!["@anna:x", "@ben:x"],
        "the author and who 👍'd"
    );
    let later = &threads[1];
    assert!(
        later.text.contains("not reached on 1 Nov") && later.text.contains("Sun 6 Dec"),
        "{}",
        later.text
    );
    assert!(later.users.is_empty(), "no pings for a carry-over");
    assert_eq!(topic(e, 1).status, Status::Done);
    assert!(last_edit_of(e, &card_event)
        .unwrap()
        .contains("Decided at the plenum on 1 Nov"));
    let t2 = topic(e, 2);
    assert_eq!(
        (t2.status, t2.carried, t2.placed.clone()),
        (Status::Open, 1, None)
    );

    // Done is done; the carried topic goes into the next minutes.
    let n = posts(e).len();
    e.tick(at(2026, 11, 2, 10, 0)).await;
    assert_eq!(posts(e).len(), n);
    e.tick(at(2026, 12, 3, 19, 0)).await;
    let text = doc(e, "House plenum 2026-12-06");
    assert!(text.contains("### #2 Garden day in spring"), "{text}");
    assert!(text.contains("not reached on 1 Nov"), "{text}");
    assert!(!text.contains("### #1 Washing"), "{text}");
    assert!(!topic(e, 2).short_notice, "carried over, not short notice");
}

#[tokio::test]
async fn only_the_author_organisers_and_admins_change_a_topic() {
    let t = setup();
    let e = &t.e;
    say(
        e,
        &anna(),
        "!top Quiet hours",
        "$a",
        at(2026, 10, 20, 12, 0),
    )
    .await;
    let refused = say(e, &ben(), "!top #1 withdraw", "$b", at(2026, 10, 20, 12, 1)).await;
    assert!(
        refused.contains("Only Anna, who brought in #1"),
        "{refused}"
    );
    assert!(topic(e, 1).is_open());
    // 🤝 from Ben does nothing, Anna's makes it one to decide
    let card = topic(e, 1).card_event.unwrap();
    e.chat
        .reactions
        .lock()
        .unwrap()
        .insert(card.clone(), vec![("@ben:x".into(), "🤝".into())]);
    e.note(Inbound::Reaction {
        room: room(),
        target: card.clone(),
    });
    e.tick(at(2026, 10, 20, 12, 2)).await;
    assert!(!topic(e, 1).to_decide());
    e.chat.reactions.lock().unwrap().insert(
        card.clone(),
        vec![
            ("@ben:x".into(), "🤝".into()),
            ("@anna:x".into(), "🤝\u{fe0f}".into()),
        ],
    );
    e.note(Inbound::Reaction {
        room: room(),
        target: card.clone(),
    });
    e.tick(at(2026, 10, 20, 12, 3)).await;
    assert!(topic(e, 1).to_decide());
    assert!(last_edit_of(e, &card).unwrap().contains("to decide"));
    // a reaction in another room for that event changes nothing
    e.chat
        .reactions
        .lock()
        .unwrap()
        .insert(card.clone(), vec![]);
    e.note(Inbound::Reaction {
        room: "!other:x".try_into().unwrap(),
        target: card.clone(),
    });
    e.tick(at(2026, 10, 20, 12, 4)).await;
    assert!(topic(e, 1).to_decide());
    // an admin may; so may Anna herself, and she brings it back
    let done = say(
        e,
        &person("@admin:x", "Admin"),
        "!top #1 later",
        "$c",
        at(2026, 10, 20, 12, 5),
    )
    .await;
    assert!(done.contains("moves to the plenum after"), "{done}");
    assert!(topic(e, 1).not_after.is_some());
    let gone = say(
        e,
        &anna(),
        "!top #1 withdraw",
        "$d",
        at(2026, 10, 20, 12, 6),
    )
    .await;
    assert!(gone.contains("withdrawn"), "{gone}");
    let back = say(e, &anna(), "!top #1 again", "$e", at(2026, 10, 20, 12, 7)).await;
    assert!(back.contains("next agenda again"), "{back}");
    assert!(topic(e, 1).is_open() && topic(e, 1).not_after.is_none());
    assert!(
        say(e, &anna(), "!top #9 done", "$f", at(2026, 10, 20, 12, 8))
            .await
            .contains("no topic #9")
    );
}

#[tokio::test]
async fn organisers_act_on_others_topics_and_everyone_else_does_not() {
    let mut p = plenum();
    p.organizers = HashSet::from(["@chris:x".try_into().unwrap()]);
    let t = setup_with(p);
    let e = &t.e;
    say(e, &anna(), "!top Bikes", "$a", at(2026, 10, 20, 12, 0)).await;
    let chris = person("@chris:x", "Chris");
    assert!(
        say(e, &chris, "!top #1 decide", "$b", at(2026, 10, 20, 12, 1))
            .await
            .contains("to decide")
    );
    assert!(
        say(e, &ben(), "!top #1 talk", "$c", at(2026, 10, 20, 12, 2))
            .await
            .contains("Only")
    );
    assert!(topic(e, 1).to_decide());
}

#[tokio::test]
async fn edits_deletions_and_proposals_count_from_the_right_person_and_room() {
    let t = setup();
    let e = &t.e;
    say(
        e,
        &anna(),
        "!top Washing machine",
        "$src",
        at(2026, 10, 20, 12, 0),
    )
    .await;
    let card = topic(e, 1).card_event.unwrap();
    let other: OwnedRoomId = "!other:x".try_into().unwrap();
    let anna_id: OwnedUserId = "@anna:x".try_into().unwrap();
    let ben_id: OwnedUserId = "@ben:x".try_into().unwrap();
    // an edit by someone else, or from another room, changes nothing
    e.note(Inbound::Edit {
        room: room(),
        original: "$src".into(),
        sender: ben_id.clone(),
        body: "!top Ben's title".into(),
    });
    e.note(Inbound::Edit {
        room: other.clone(),
        original: "$src".into(),
        sender: anna_id.clone(),
        body: "!top Other room".into(),
    });
    e.tick(at(2026, 10, 20, 12, 1)).await;
    assert_eq!(topic(e, 1).title, "Washing machine");
    // Anna's own edit changes it, and the topic's message follows
    e.note(Inbound::Edit {
        room: room(),
        original: "$src".into(),
        sender: anna_id.clone(),
        body: "!top house Washing machine broken\nSince October.".into(),
    });
    e.tick(at(2026, 10, 20, 12, 2)).await;
    assert_eq!(topic(e, 1).title, "Washing machine broken");
    assert_eq!(topic(e, 1).details, "Since October.");
    assert!(last_edit_of(e, &card)
        .unwrap()
        .contains("Washing machine broken"));
    // a proposal in the thread: Ben's is just a message, Anna's is the proposal
    e.note(Inbound::Thread {
        room: room(),
        root: card.clone(),
        event: "$p1".into(),
        sender: ben_id.clone(),
        body: "Proposal: do nothing".into(),
    });
    e.tick(at(2026, 10, 20, 12, 3)).await;
    assert_eq!(topic(e, 1).proposal, None);
    e.note(Inbound::Thread {
        room: room(),
        root: card.clone(),
        event: "$p2".into(),
        sender: anna_id.clone(),
        body: "Vorschlag: eine neue kaufen".into(),
    });
    e.tick(at(2026, 10, 20, 12, 4)).await;
    assert_eq!(topic(e, 1).proposal.as_deref(), Some("eine neue kaufen"));
    // her edit of it counts, Ben's edit of it does not
    e.note(Inbound::Edit {
        room: room(),
        original: "$p2".into(),
        sender: ben_id.clone(),
        body: "Vorschlag: nichts".into(),
    });
    e.note(Inbound::Edit {
        room: room(),
        original: "$p2".into(),
        sender: anna_id.clone(),
        body: "Vorschlag: eine gebrauchte kaufen".into(),
    });
    e.tick(at(2026, 10, 20, 12, 5)).await;
    assert_eq!(
        topic(e, 1).proposal.as_deref(),
        Some("eine gebrauchte kaufen")
    );
    // a deletion from another room or by Ben does nothing; Anna deleting her message
    // withdraws it
    e.note(Inbound::Redaction {
        room: other,
        redacts: "$src".into(),
        sender: anna_id.clone(),
    });
    e.note(Inbound::Redaction {
        room: room(),
        redacts: "$src".into(),
        sender: ben_id,
    });
    e.tick(at(2026, 10, 20, 12, 6)).await;
    assert!(topic(e, 1).is_open());
    e.note(Inbound::Redaction {
        room: room(),
        redacts: "$src".into(),
        sender: anna_id,
    });
    e.tick(at(2026, 10, 20, 12, 7)).await;
    let t1 = topic(e, 1);
    assert_eq!(t1.status, Status::Withdrawn);
    assert!(t1.deleted && t1.details.is_empty() && t1.proposal.is_none());
    assert!(last_edit_of(e, &card).unwrap().contains("Withdrawn"));
    assert!(
        say(e, &anna(), "!top #1 again", "$x", at(2026, 10, 20, 12, 8))
            .await
            .contains("deleted")
    );
}

#[tokio::test]
async fn a_cancelled_meeting_hands_its_topics_to_the_next() {
    let t = setup();
    let e = &t.e;
    say(
        e,
        &anna(),
        "!top Heating in the attic",
        "$a",
        at(2026, 10, 20, 12, 0),
    )
    .await;
    e.tick(at(2026, 10, 29, 19, 0)).await;
    assert_eq!(
        topic(e, 1).placed.as_deref(),
        Some("house-series#2026-11-01")
    );
    // the meeting leaves the calendar
    let list: Vec<Occurrence> = first_sundays()
        .into_iter()
        .filter(|o| o.id != "house-series#2026-11-01")
        .collect();
    e.apply_calendar("house", list, at(2026, 10, 30, 9, 0))
        .await;
    e.tick(at(2026, 10, 30, 9, 1)).await;
    let notice = e
        .chat
        .texts()
        .into_iter()
        .find(|t| t.contains("cancelled"))
        .expect("a notice");
    assert!(
        notice.contains("Its topics (#1) move to the next plenum"),
        "{notice}"
    );
    assert_eq!(topic(e, 1).placed, None);
    let card = topic(e, 1).card_event.unwrap();
    assert!(last_edit_of(e, &card).unwrap().contains("Sun 6 Dec, 19:00"));
    e.tick(at(2026, 12, 3, 19, 0)).await;
    assert!(doc(e, "House plenum 2026-12-06").contains("### #1 Heating in the attic"));
}

#[tokio::test]
async fn a_restart_or_a_failure_sends_nothing_twice() {
    let t = setup();
    let e = &t.e;
    *e.chat.fail.lock().unwrap() = true;
    let answer = say(e, &anna(), "!top Bins", "$a", at(2026, 10, 20, 12, 0)).await;
    assert!(answer.contains("#1 is noted"), "{answer}");
    *e.chat.fail.lock().unwrap() = false;
    e.tick(at(2026, 10, 20, 12, 1)).await;
    e.tick(at(2026, 10, 20, 12, 2)).await;
    assert_eq!(posts(e).len(), 1, "one message, posted once");
    // the same message again (a sync after a restart) is no second topic
    assert_eq!(
        say(e, &anna(), "!top Bins", "$a", at(2026, 10, 20, 12, 3)).await,
        ""
    );
    assert_eq!(
        e.state.try_lock().unwrap().book.plenums["house"]
            .topics
            .len(),
        1
    );
    e.tick(at(2026, 10, 29, 19, 0)).await;
    edit_doc(e, "House plenum 2026-11-01", |t| {
        *t = t.replace("### #1 Bins", "### #1 Bins\n\nDecision: yellow bins")
    });
    e.tick(at(2026, 11, 2, 9, 0)).await;
    assert_eq!(threads(e).len(), 1);
    // a restart: a new engine from the files (its chat has seen nothing)
    let state = State::load(&t.dir.path().join("state.json")).await.unwrap();
    assert_eq!(state.book.plenums["house"].topics[&1].status, Status::Done);
    let again = engine(plenum(), t.dir.path(), state);
    again.tick(at(2026, 11, 2, 9, 5)).await;
    assert!(threads(&again).is_empty(), "nothing is said again");
}

#[tokio::test]
async fn a_failing_thread_holds_up_nothing_and_is_given_up() {
    let t = setup();
    let e = &t.e;
    say(e, &anna(), "!top Bins", "$a", at(2026, 10, 20, 12, 0)).await;
    e.tick(at(2026, 10, 29, 19, 0)).await;
    edit_doc(e, "House plenum 2026-11-01", |t| {
        *t = t.replace("### #1 Bins", "### #1 Bins\n\nNotes only.")
    });
    *e.chat.fail_threads.lock().unwrap() = true;
    for m in 0..5 {
        e.tick(at(2026, 11, 2, 9, m)).await;
    }
    // the summary went out; the thread was tried three times, then the topic went on
    assert!(
        e.chat
            .texts()
            .iter()
            .any(|t| t.contains("Minutes:") || t.contains("closed")),
        "{:?}",
        e.chat.texts()
    );
    let t1 = topic(e, 1);
    assert_eq!(t1.outcome_attempts, topics::OUTCOME_ATTEMPTS);
    assert_eq!(t1.status, Status::Done, "talked about");
    assert!(!t1.history[0].told);
    assert!(threads(e).is_empty());
    // its message does not claim the thread has more
    let card = t1.card_event.unwrap();
    assert!(!last_edit_of(e, &card)
        .unwrap()
        .contains("The thread has more"));
}

#[tokio::test]
async fn what_a_confidential_meeting_shares_of_its_topics() {
    let t = setup();
    let e = &t.e;
    say(
        e,
        &anna(),
        "!top Rent arrears",
        "$a",
        at(2026, 10, 20, 12, 0),
    )
    .await;
    say(
        e,
        &anna(),
        "!minutes confidential",
        "$m",
        at(2026, 10, 20, 12, 1),
    )
    .await;
    e.tick(at(2026, 10, 29, 19, 0)).await;
    let announcement = posts(e)
        .into_iter()
        .find(|p| p.text.contains("confidential"))
        .unwrap();
    assert!(
        !announcement.text.contains("Rent arrears"),
        "{}",
        announcement.text
    );
    edit_doc(e, "House plenum 2026-11-01", |t| {
        *t = t.replace(
            "### #1 Rent arrears",
            "### #1 Rent arrears\n\nDecision: a payment plan for X",
        )
    });
    e.tick(at(2026, 11, 2, 9, 0)).await;
    let thread = threads(e).pop().unwrap();
    assert!(
        thread.text.contains("dealt with at the plenum on 1 Nov"),
        "{}",
        thread.text
    );
    assert!(
        !thread.text.contains("payment plan") && !thread.text.contains("wiki.test"),
        "{}",
        thread.text
    );
}

#[tokio::test]
async fn a_section_deleted_from_the_minutes_carries_nothing_on() {
    let t = setup();
    let e = &t.e;
    say(e, &anna(), "!top Bins", "$a", at(2026, 10, 20, 12, 0)).await;
    e.tick(at(2026, 10, 29, 19, 0)).await;
    edit_doc(e, "House plenum 2026-11-01", |t| {
        *t = t.replace("### #1 Bins", "Someone rewrote it all.")
    });
    e.tick(at(2026, 11, 2, 9, 0)).await;
    let t1 = topic(e, 1);
    assert_eq!(t1.status, Status::Unclear);
    assert!(threads(e).is_empty(), "no message about it");
    let card = t1.card_event.unwrap();
    assert!(last_edit_of(e, &card).unwrap().contains("`!top #1 again`"));
}

#[tokio::test]
async fn a_plenum_changes_its_own_settings_in_its_room() {
    let t = setup();
    let e = &t.e;
    let shown = say(
        e,
        &anna(),
        "!plenum settings",
        "$s",
        at(2026, 10, 20, 12, 0),
    )
    .await;
    assert!(shown.contains("`agenda_days`: **3** (default)"), "{shown}");
    let wrong = say(
        e,
        &anna(),
        "!plenum set agenda_days 40",
        "$w",
        at(2026, 10, 20, 12, 1),
    )
    .await;
    assert!(wrong.contains("from 1 to 14"), "{wrong}");
    let changed = say(
        e,
        &anna(),
        "!plenum set agenda_days 5",
        "$c",
        at(2026, 10, 20, 12, 2),
    )
    .await;
    assert!(
        changed.contains("Anna changed `agenda_days` of House plenum: 3 → **5**"),
        "{changed}"
    );
    // five days before now, no longer three
    e.tick(at(2026, 10, 27, 19, 0)).await;
    assert!(e.wiki.by_title("House plenum 2026-11-01").is_some());
    let same = say(
        e,
        &anna(),
        "!plenum set agenda_days 5",
        "$d",
        at(2026, 10, 27, 19, 1),
    )
    .await;
    assert!(same.contains("already"), "{same}");
    let back = say(
        e,
        &anna(),
        "!plenum reset agenda_days",
        "$r",
        at(2026, 10, 27, 19, 2),
    )
    .await;
    assert!(back.contains("5 → **3** (the default again)"), "{back}");
    // topics off in the room: `!top` as before
    say(
        e,
        &anna(),
        "!plenum set topics off",
        "$o",
        at(2026, 10, 30, 12, 0),
    )
    .await;
    let old = say(e, &anna(), "!top Coffee", "$k", at(2026, 10, 30, 12, 1)).await;
    assert!(old.contains("On the agenda"), "{old}");
}

#[tokio::test]
async fn settings_belong_to_the_organisers_where_there_are_some() {
    let mut p = plenum();
    p.organizers = HashSet::from(["@chris:x".try_into().unwrap()]);
    let t = setup_with(p);
    let e = &t.e;
    let refused = say(
        e,
        &anna(),
        "!plenum set reminder 0",
        "$a",
        at(2026, 10, 20, 12, 0),
    )
    .await;
    assert!(
        refused.contains("Only the plenum's organizers"),
        "{refused}"
    );
    let ok = say(
        e,
        &person("@chris:x", "Chris"),
        "!plenum set reminder 0",
        "$b",
        at(2026, 10, 20, 12, 1),
    )
    .await;
    assert!(ok.contains("→ **0**"), "{ok}");
    // house-infra decides that rooms set nothing
    let mut fixed = plenum();
    fixed.topics.room_settings = false;
    let t = setup_with(fixed);
    let no = say(
        &t.e,
        &anna(),
        "!plenum set reminder 0",
        "$c",
        at(2026, 10, 20, 12, 0),
    )
    .await;
    assert!(no.contains("set centrally"), "{no}");
}

#[tokio::test]
async fn an_admin_changes_a_setting_from_their_direct_chat_and_the_room_hears_it() {
    let t = setup();
    let e = &t.e;
    let dm: OwnedRoomId = "!dm:x".try_into().unwrap();
    let admin = person("@admin:x", "Admin");
    let cmd = parse_for("!plenum set house carry_over off", &["house"]).unwrap();
    let answer = e
        .command_in(&dm, &admin, cmd, at(2026, 10, 20, 12, 0), Scope::AdminDm)
        .await;
    assert!(answer.contains("off"), "{answer}");
    assert!(e
        .chat
        .sent
        .lock()
        .unwrap()
        .iter()
        .any(|(r, t, _)| r == ROOM && t.contains("carry_over")));
}

#[tokio::test]
async fn items_from_before_topics_become_topics() {
    let dir = tempfile::tempdir().unwrap();
    let mut st = State::default();
    let ps = st.plenums.entry("house".into()).or_default();
    ps.occurrences = first_sundays();
    ps.calendar_read = Some(at(2026, 10, 1, 0, 0));
    ps.pending_tops.push(crate::state::Top {
        text: "Old item".into(),
        by: "Dana".into(),
        at: at(2026, 10, 10, 10, 0),
    });
    let e = engine(plenum(), dir.path(), st);
    e.tick(at(2026, 10, 29, 19, 0)).await;
    let text = doc(&e, "House plenum 2026-11-01");
    assert!(text.contains("* #1 Old item (Dana)"), "{text}");
    assert!(text.contains("### #1 Old item"), "{text}");
    assert!(e.state.try_lock().unwrap().plenums["house"]
        .pending_tops
        .is_empty());
    // no message of its own: none is posted for it, ever
    e.tick(at(2026, 10, 29, 19, 1)).await;
    assert!(posts(&e).iter().all(|p| !p.text.contains("📌")));
}

#[tokio::test]
async fn a_reply_brings_in_someone_elses_message_by_its_first_line() {
    let t = setup();
    let e = &t.e;
    let origin = Origin {
        event: Some("$ben-top".into()),
        from: Some("Chris".into()),
        from_user: Some("@chris:x".try_into().unwrap()),
        from_event: Some("$chris-msg".into()),
    };
    let cmd = Command::Top {
        key: None,
        text: "decide: Party on Saturday\nAll the details of Chris's private plans".into(),
    };
    let answer = e
        .command_from(
            &room(),
            &ben(),
            cmd,
            at(2026, 10, 20, 12, 0),
            Scope::Room,
            &origin,
        )
        .await;
    assert_eq!(answer, "");
    let t1 = topic(e, 1);
    assert_eq!(t1.title, "Party on Saturday");
    assert!(t1.details.is_empty() && !t1.decide);
    assert_eq!((t1.by.as_str(), t1.from.as_deref()), ("Ben", Some("Chris")));
    assert!(
        posts(e)[0].text.contains("from Ben (Chris’s message)"),
        "{}",
        posts(e)[0].text
    );
}

#[tokio::test]
async fn limits_keep_a_flood_out() {
    let t = setup();
    let e = &t.e;
    for i in 0..topics::PER_PERSON_A_DAY {
        say(
            e,
            &anna(),
            &format!("!top Topic {i}"),
            &format!("$t{i}"),
            at(2026, 10, 20, 12, 0),
        )
        .await;
    }
    let refused = say(
        e,
        &anna(),
        "!top One more",
        "$more",
        at(2026, 10, 20, 13, 0),
    )
    .await;
    assert!(refused.contains("more tomorrow"), "{refused}");
    let ok = say(
        e,
        &anna(),
        "!top Next day",
        "$next",
        at(2026, 10, 21, 13, 0),
    )
    .await;
    assert_eq!(ok, "");
}

#[tokio::test]
async fn the_bots_marker_in_a_topic_changes_nothing_in_the_minutes() {
    let t = setup();
    let e = &t.e;
    let marker = topics::SECTIONS_MARKER;
    say(
        e,
        &ben(),
        &format!("!top Garden\n{marker}"),
        "$a",
        at(2026, 10, 20, 12, 0),
    )
    .await;
    say(
        e,
        &ben(),
        &format!("!top {marker}"),
        "$b",
        at(2026, 10, 20, 12, 1),
    )
    .await;
    e.tick(at(2026, 10, 29, 19, 0)).await;
    say(
        e,
        &anna(),
        "!top Washing machine",
        "$c",
        at(2026, 10, 30, 9, 0),
    )
    .await;
    e.tick(at(2026, 10, 30, 9, 1)).await;
    let text = doc(e, "House plenum 2026-11-01");
    assert_eq!(text.matches(marker).count(), 1, "{text}");
    let sections = topics::sections(&text);
    assert_eq!(
        sections.keys().copied().collect::<Vec<_>>(),
        vec![1, 2, 3],
        "{text}"
    );
    assert!(text.contains("### #3 Washing machine"), "{text}");
}

#[tokio::test]
async fn an_edit_back_to_an_earlier_text_still_reaches_the_room() {
    let t = setup();
    let e = &t.e;
    say(e, &anna(), "!top Bikes", "$a", at(2026, 10, 20, 12, 0)).await;
    let card = topic(e, 1).card_event.unwrap();
    // to decide, to talk about, to decide again: three edits, each its own transaction
    for (i, action) in ["decide", "talk", "decide"].iter().enumerate() {
        say(
            e,
            &anna(),
            &format!("!top #1 {action}"),
            &format!("$x{i}"),
            at(2026, 10, 20, 12, 1),
        )
        .await;
        e.tick(at(2026, 10, 20, 12, 2 + i as u32)).await;
    }
    let shown = last_edit_of(e, &card).unwrap();
    assert!(shown.contains("to decide"), "{shown}");
    assert_eq!(e.chat.edits.lock().unwrap().len(), 3);
}

#[tokio::test]
async fn items_moved_before_a_crash_are_not_moved_twice() {
    let dir = tempfile::tempdir().unwrap();
    let mut st = State::default();
    let ps = st.plenums.entry("house".into()).or_default();
    ps.occurrences = first_sundays();
    ps.calendar_read = Some(at(2026, 10, 1, 0, 0));
    let item = crate::state::Top {
        text: "Old item".into(),
        by: "Dana".into(),
        at: at(2026, 10, 10, 10, 0),
    };
    ps.pending_tops.push(item.clone());
    // topics.json was saved with it moved, state.json not (a crash between the two files)
    let area = st.book.plenums.entry("house".into()).or_default();
    area.last = 1;
    area.migrated = true;
    area.topics.insert(
        1,
        Topic {
            number: 1,
            title: "Old item".into(),
            by: "Dana".into(),
            at: item.at,
            card: crate::state::Step {
                txn: None,
                done: true,
            },
            buttons: true,
            ..Default::default()
        },
    );
    let e = engine(plenum(), dir.path(), st);
    e.tick(at(2026, 10, 20, 12, 0)).await;
    let st = e.state.try_lock().unwrap();
    assert_eq!(st.book.plenums["house"].topics.len(), 1);
    assert!(st.plenums["house"].pending_tops.is_empty());
}

#[tokio::test]
async fn minutes_written_the_old_way_carry_nothing_on() {
    let t = setup();
    let e = &t.e;
    say(e, &anna(), "!top Bins", "$a", at(2026, 10, 20, 12, 0)).await;
    say(e, &ben(), "!top Garden", "$b", at(2026, 10, 20, 12, 1)).await;
    e.tick(at(2026, 10, 29, 19, 0)).await;
    // the minute-taker wrote everything at the end, not in the topics' sections
    edit_doc(e, "House plenum 2026-11-01", |t| {
        t.push_str("\n### Bins\n\nWe talked.\n\nDecision: yellow bins\n")
    });
    e.tick(at(2026, 11, 2, 9, 0)).await;
    assert_eq!(topic(e, 1).status, Status::Unclear);
    assert_eq!(topic(e, 2).status, Status::Unclear);
    assert!(
        threads(e).is_empty(),
        "nobody is told a wrong \"not reached\""
    );
}

#[tokio::test]
async fn without_carry_over_a_topic_not_reached_says_how_to_bring_it_back() {
    let mut p = plenum();
    p.topics.carry_over = false;
    let t = setup_with(p);
    let e = &t.e;
    say(e, &anna(), "!top Bins", "$a", at(2026, 10, 20, 12, 0)).await;
    e.tick(at(2026, 10, 29, 19, 0)).await;
    e.tick(at(2026, 11, 2, 9, 0)).await;
    let thread = threads(e).pop().expect("a note");
    assert!(thread.text.contains("`!top #1 again`"), "{}", thread.text);
    assert!(thread.users.is_empty());
    assert_eq!(topic(e, 1).status, Status::Unclear);
}

#[tokio::test]
async fn the_quoted_person_may_withdraw_and_their_deletion_counts() {
    let t = setup();
    let e = &t.e;
    let origin = Origin {
        event: Some("$ben-top".into()),
        from: Some("Chris".into()),
        from_user: Some("@chris:x".try_into().unwrap()),
        from_event: Some("$chris-msg".into()),
    };
    let cmd = Command::Top {
        key: None,
        text: "Party on Saturday".into(),
    };
    e.command_from(
        &room(),
        &ben(),
        cmd,
        at(2026, 10, 20, 12, 0),
        Scope::Room,
        &origin,
    )
    .await;
    // Chris deletes the message Ben brought in: the topic goes, with its words
    e.note(Inbound::Redaction {
        room: room(),
        redacts: "$chris-msg".into(),
        sender: "@chris:x".try_into().unwrap(),
    });
    e.tick(at(2026, 10, 20, 12, 1)).await;
    let t1 = topic(e, 1);
    assert_eq!(t1.status, Status::Withdrawn);
    assert_eq!(t1.title, "(deleted)");
    let card = t1.card_event.unwrap();
    let shown = last_edit_of(e, &card).unwrap();
    assert!(!shown.contains("Party"), "{shown}");
}

#[tokio::test]
async fn texts_go_after_their_time_also_with_topics_off() {
    let t = setup();
    let e = &t.e;
    say(
        e,
        &anna(),
        "!top Bins\nA long story.",
        "$a",
        at(2026, 10, 20, 12, 0),
    )
    .await;
    say(e, &anna(), "!top #1 done", "$b", at(2026, 10, 20, 12, 1)).await;
    say(
        e,
        &ben(),
        "!plenum set topics off",
        "$c",
        at(2026, 10, 20, 12, 2),
    )
    .await;
    e.tick(at(2026, 12, 1, 12, 0)).await;
    assert!(topic(e, 1).details.is_empty());
}

#[tokio::test]
async fn a_confidential_outcome_leaves_the_topics_message_saying_little() {
    let t = setup();
    let e = &t.e;
    say(
        e,
        &anna(),
        "!top Rent arrears",
        "$a",
        at(2026, 10, 20, 12, 0),
    )
    .await;
    say(
        e,
        &anna(),
        "!minutes confidential",
        "$m",
        at(2026, 10, 20, 12, 1),
    )
    .await;
    e.tick(at(2026, 10, 29, 19, 0)).await;
    edit_doc(e, "House plenum 2026-11-01", |t| {
        *t = t.replace(
            "### #1 Rent arrears",
            "### #1 Rent arrears\n\nDecision: a plan",
        )
    });
    e.tick(at(2026, 11, 2, 9, 0)).await;
    let card = topic(e, 1).card_event.unwrap();
    let shown = last_edit_of(e, &card).unwrap();
    assert!(
        shown.contains("Dealt with at the plenum on 1 Nov"),
        "{shown}"
    );
    assert!(!shown.contains("Decided"), "{shown}");
}

#[tokio::test]
async fn the_agenda_comes_first_in_the_announcement() {
    let t = setup();
    let e = &t.e;
    say(e, &anna(), "!top Bins", "$a", at(2026, 10, 20, 12, 0)).await;
    e.tick(at(2026, 10, 29, 19, 0)).await;
    let a = posts(e)
        .into_iter()
        .find(|p| p.text.contains("📋"))
        .unwrap()
        .text;
    let agenda = a.find("**To talk about:**").unwrap();
    let link = a.find("Agenda and minutes").unwrap();
    assert!(agenda < link, "{a}");
    assert!(!a.contains("write it into the document"), "{a}");
}
