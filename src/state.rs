//! What the bot remembers between restarts (`store/state.json`).
//!
//! Every step of a meeting is recorded as soon as it is done, and a message's transaction
//! ID before it is sent: a restart in the middle repeats nothing that already happened
//! (the homeserver drops a resend with the same transaction ID) and skips nothing.

use std::{collections::BTreeMap, path::Path};

use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use mxbot_common::membership::{Notices, Remembered};

use crate::{calendar::Occurrence, config::Publication, schedule::MeetingKey};

/// Meetings kept per plenum; older ones are dropped (their protocols stay in Outline).
const KEEP_MEETINGS: usize = 60;

/// Decisions the overview shows (the full list is the decision document).
pub const KEEP_DECISIONS: usize = 10;

/// Withheld tasks kept per plenum (the newest): far more than ever stay open at once.
const KEEP_WITHHELD: usize = 200;

#[derive(Serialize, Deserialize, Default, Debug)]
pub struct State {
    #[serde(default)]
    pub plenums: BTreeMap<String, PlenumState>,
    /// Per room ID: whom the bot invited there, failed invitations, removals held and the
    /// running handovers (a member's other accounts kept for a while after their choice;
    /// rooms of plenums with a group).
    #[serde(default)]
    pub rooms: BTreeMap<String, Remembered>,
    /// Per person (the directory's id): asked to choose an account, admins told about them.
    #[serde(default)]
    pub people: Notices,
    /// Link requests from the provider's flow the bot took up, by the request's key
    /// ([`mxbot_common::directory::LinkRequest::key`]: person, account, time), for a day: a
    /// restart sends no invitation or code twice, and the day's deliveries are counted.
    #[serde(default)]
    pub link_requests: BTreeMap<String, LinkDelivery>,
    /// Per person (the directory's id): what the admins were told about the end of their stay
    /// (`stay_until`; [`crate::stay`]). Left out when empty, so an older image reads the file.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub stays: crate::stay::Memory,
    /// The rooms' own settings (`store/settings.json` beside the state, a file of its own so
    /// an older image after a rollback neither drops nor overwrites them).
    #[serde(skip)]
    pub settings: crate::plenum_settings::Store,
}

/// One link request the bot took up ([`crate::link_delivery`]). Holds no code: the code is
/// made when it is sent.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct LinkDelivery {
    /// The person who asked (the directory's id) and their login (all the account is told of
    /// them; the name they show is not kept).
    pub person: String,
    pub username: String,
    /// The account to link.
    pub account: String,
    /// When the person asked, and when the bot took it up.
    pub requested_at: DateTime<Utc>,
    pub handled_at: DateTime<Utc>,
    pub outcome: LinkOutcome,
    /// The direct chat with the account (once opened).
    #[serde(default)]
    pub room: Option<String>,
    /// The first message (who asked) is in it.
    #[serde(default)]
    pub introduced: bool,
    /// Failed attempts at opening the chat or sending the code.
    #[serde(default)]
    pub attempts: u32,
    /// Why it was refused, skipped or given up.
    #[serde(default)]
    pub reason: Option<String>,
    /// A refused or skipped record stands for the person's earlier ones too (one per person,
    /// [`crate::link_delivery`]): how many it replaced.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub repeats: u32,
}

fn is_zero(n: &u32) -> bool {
    *n == 0
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LinkOutcome {
    /// The account is (being) invited; the code goes out once it has joined.
    Pending,
    /// The code went out.
    Sent,
    /// Not joined in time, the request withdrawn or replaced, or Matrix failed too often.
    GaveUp,
    /// Over a rate limit: nobody was invited.
    Skipped,
    /// Not to be served (linked already, someone else's account, the house's server).
    Refused,
}

#[derive(Serialize, Deserialize, Default, Debug)]
pub struct PlenumState {
    #[serde(default)]
    pub collection_id: Option<String>,
    #[serde(default)]
    pub template_doc_id: Option<String>,
    #[serde(default)]
    pub decisions_doc_id: Option<String>,
    /// The overview the bot keeps up to date, the rules people keep, and the document the
    /// protocols are filed under.
    #[serde(default)]
    pub overview_doc_id: Option<String>,
    #[serde(default)]
    pub rules_doc_id: Option<String>,
    #[serde(default)]
    pub protocols_doc_id: Option<String>,
    /// An example of finished minutes, to show how they are written.
    #[serde(default)]
    pub example_doc_id: Option<String>,
    /// The documents' paths in the wiki (`/doc/…`), by role: overview, rules, protocols,
    /// decisions, template.
    #[serde(default)]
    pub urls: BTreeMap<String, String>,
    /// The overview's text as last written (it is only written when it changes).
    #[serde(default)]
    pub overview_text: Option<String>,
    /// The open pages (document IDs) as last pinned in the collection (pinned again only when
    /// they change: a pin someone removed stays removed).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pinned: Option<Vec<String>>,
    /// When the collection, its access and its documents were last checked.
    #[serde(default)]
    pub area_checked: Option<DateTime<Utc>>,
    /// The open tasks of the last closed protocol: they go into the next one.
    #[serde(default)]
    pub open_tasks: Vec<Carried>,
    /// The meeting `open_tasks` are from: a meeting closed later but held earlier (two close
    /// together) does not replace them.
    #[serde(default)]
    pub open_tasks_from: Option<MeetingKey>,
    /// The latest decisions (newest last), for the overview.
    #[serde(default)]
    pub decisions: Vec<Carried>,
    /// The open tasks of closed meetings whose minutes did not go into the chat (link only,
    /// confidential), as read from them. They come along to the next minutes like any task;
    /// a later meeting's summary leaves out every task that contains one of them.
    #[serde(default)]
    pub withheld_tasks: Vec<String>,
    /// The calendar's occurrences as last read (kept while the calendar cannot be read).
    #[serde(default)]
    pub occurrences: Vec<Occurrence>,
    /// When the calendar was last read; None: never (nothing is known about the dates yet).
    #[serde(default)]
    pub calendar_read: Option<DateTime<Utc>>,
    /// The calendar was made anew (it was missing): its reads count only once it has events.
    #[serde(default)]
    pub calendar_remade: bool,
    /// Agenda items sent before the next protocol existed; they go into it.
    #[serde(default)]
    pub pending_tops: Vec<Top>,
    /// Topics of a closed or cancelled page that were not reached, waiting for the next page
    /// (they go into it when it is made, or at once if it is made already, and are dropped
    /// from here then).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub open_topics: Vec<CarriedTopic>,
    /// By the calendar occurrence's ID.
    #[serde(default)]
    pub meetings: BTreeMap<String, Meeting>,
}

/// A topic carried onto the next page (docs/PLENUM-PAGES.md §4.4).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct CarriedTopic {
    /// The block's heading line (a page that has it has the topic already).
    pub heading: String,
    /// The Markdown that goes into the page.
    pub block: String,
    /// The meeting it comes from, and what of that one went into the room: a meeting that
    /// receives it shares no more than that.
    pub from: MeetingKey,
    pub publication: Publication,
}

/// The message with a page's link (`messages quiet` edits it when the meeting moves or is
/// cancelled).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
pub struct LinkMessage {
    pub event: String,
    pub text: String,
    /// Edits sent (each its own transaction).
    #[serde(default)]
    pub edits: u32,
}

/// A task or a decision from a closed protocol: its text, the meeting's day and protocol.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Carried {
    pub text: String,
    pub date: chrono::NaiveDate,
    #[serde(default)]
    pub link: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Top {
    pub text: String,
    pub by: String,
    pub at: DateTime<Utc>,
}

/// One meeting and how far its protocol got.
#[derive(Serialize, Deserialize, Default, Clone, Debug)]
pub struct Meeting {
    /// Chosen before the document is created, so a restart finds it instead of creating a
    /// second one.
    pub doc_id: String,
    #[serde(default)]
    pub created: bool,
    /// The document's path in Outline (`/doc/…`).
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub announce: Step,
    #[serde(default)]
    pub remind: Step,
    #[serde(default)]
    pub close: Close,
    /// The meeting ended before the bot got to prepare it (it was down): nothing happens.
    #[serde(default)]
    pub skipped: bool,
    /// When it starts and ends (local), as the calendar says now.
    #[serde(default)]
    pub start: Option<MeetingKey>,
    #[serde(default)]
    pub end: Option<chrono::NaiveDateTime>,
    /// The protocol's title as made (it follows a move).
    #[serde(default)]
    pub title: Option<String>,
    /// Gone from the calendar before it began: no protocol steps any more (it comes back if
    /// the occurrence does).
    #[serde(default)]
    pub cancelled: bool,
    #[serde(default)]
    pub cancel_notice: Step,
    /// Moved in the calendar after it was announced: the new announcement says from when.
    #[serde(default)]
    pub moved_from: Option<MeetingKey>,
    /// The room was told about it (announced, or told it is cancelled) and then it moved or
    /// came back: announced again right away, not only `prepare_days` before.
    #[serde(default)]
    pub announce_again: bool,
    /// What of its minutes goes into the room, as set in the chat (`!minutes confidential`);
    /// None: the plenum's `publication`. Every step reads it when it sends, so a change
    /// reaches the steps still to come, after a failed try or a restart too.
    #[serde(default)]
    pub publication: Option<Publication>,
    /// Who set `publication`: they may take it back.
    #[serde(default)]
    pub publication_by: Option<String>,
    /// The link message, once posted (with `post`; an older image's has none).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link: Option<LinkMessage>,
}

/// A message that is sent once: its transaction ID, then whether it went out.
#[derive(Serialize, Deserialize, Default, Clone, Debug)]
pub struct Step {
    #[serde(default)]
    pub txn: Option<String>,
    #[serde(default)]
    pub done: bool,
}

impl Step {
    /// The transaction ID to send with (created and kept on the first call).
    pub fn txn(&mut self) -> String {
        self.txn
            .get_or_insert_with(|| format!("plenum-{}", uuid::Uuid::new_v4()))
            .clone()
    }
}

#[derive(Serialize, Deserialize, Default, Clone, Debug)]
pub struct Close {
    /// Someone said `!minutes done`: close at the next tick, whatever the time.
    #[serde(default)]
    pub requested: bool,
    #[serde(default)]
    pub decisions_saved: bool,
    /// The open tasks went into the next protocol made already (`carry_tasks_on`).
    #[serde(default)]
    pub tasks_carried: bool,
    /// The topics not reached were read into `open_topics`.
    #[serde(default)]
    pub topics_read: bool,
    /// The result box is in the page (seen there at a tick after it was written), or given up.
    #[serde(default)]
    pub result_box: bool,
    #[serde(default)]
    pub box_attempts: u32,
    /// The PDF attached to the protocol (its URL path).
    #[serde(default)]
    pub pdf_url: Option<String>,
    #[serde(default)]
    pub pdf_linked: bool,
    /// Failed attempts at rendering, attaching and linking the PDF; after a few it is given
    /// up (`pdf_failed`) and the summary says so.
    #[serde(default)]
    pub pdf_attempts: u32,
    #[serde(default)]
    pub pdf_failed: bool,
    #[serde(default)]
    pub pdf_room: Step,
    /// Failed attempts at posting the PDF in the room; given up the same way
    /// (`pdf_room_failed`: the PDF is only in the wiki).
    #[serde(default)]
    pub pdf_room_attempts: u32,
    #[serde(default)]
    pub pdf_room_failed: bool,
    #[serde(default)]
    pub summary: Step,
    #[serde(default)]
    pub done: bool,
}

impl PlenumState {
    /// Forget the oldest finished meetings beyond [`KEEP_MEETINGS`].
    pub fn prune(&mut self) {
        while self.meetings.len() > KEEP_MEETINGS {
            let Some(oldest) = self
                .meetings
                .iter()
                .filter(|(_, m)| m.close.done || m.skipped || m.cancelled)
                .min_by_key(|(_, m)| m.start)
                .map(|(id, _)| id.clone())
            else {
                break;
            };
            self.meetings.remove(&oldest);
        }
        if self.withheld_tasks.len() > KEEP_WITHHELD {
            self.withheld_tasks
                .drain(..self.withheld_tasks.len() - KEEP_WITHHELD);
        }
        if self.decisions.len() > KEEP_DECISIONS {
            self.decisions
                .drain(..self.decisions.len() - KEEP_DECISIONS);
        }
    }
}

/// Where the rooms' settings are kept, beside the state.
pub fn settings_path(state: &Path) -> std::path::PathBuf {
    state.with_file_name("settings.json")
}

impl State {
    pub async fn load(path: &Path) -> Result<Self> {
        let mut st: Self = mxbot_common::persist::load_json_or_default(path).await?;
        st.settings = mxbot_common::persist::load_json_or_default(&settings_path(path)).await?;
        Ok(st)
    }

    pub async fn save(&self, path: &Path) -> Result<()> {
        mxbot_common::persist::save_json_atomic(&settings_path(path), &self.settings).await?;
        mxbot_common::persist::save_json_atomic(path, self).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn the_state_survives_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let mut st = State::default();
        let p = st.plenums.entry("house".into()).or_default();
        let key = "plenum-1#2026-11-01".to_owned();
        let mut m = Meeting {
            doc_id: "d1".into(),
            created: true,
            start: Some("2026-11-01 19:00".parse().unwrap()),
            ..Default::default()
        };
        let txn = m.announce.txn();
        p.meetings.insert(key.clone(), m);
        st.save(&path).await.unwrap();

        let back = State::load(&path).await.unwrap();
        let p = &back.plenums["house"];
        assert_eq!(p.meetings[&key].announce.txn.as_deref(), Some(txn.as_str()));
        assert!(p.meetings[&key].created);
        assert_eq!(
            p.meetings[&key].start.unwrap().to_string(),
            "2026-11-01 19:00"
        );
    }

    #[test]
    fn a_state_from_before_publication_settings_shares_as_before() {
        let json = r#"{"plenums":{"house":{"meetings":{"m1":{"doc_id":"d1","created":true}}}}}"#;
        let st: State = serde_json::from_str(json).unwrap();
        let ps = &st.plenums["house"];
        assert_eq!(ps.meetings["m1"].publication, None, "the plenum's default");
        assert!(ps.withheld_tasks.is_empty());
        let mut m = Meeting {
            publication: Some(Publication::Confidential),
            ..Default::default()
        };
        m.publication_by = Some("@anna:x".into());
        let back: Meeting = serde_json::from_str(&serde_json::to_string(&m).unwrap()).unwrap();
        assert_eq!(back.publication, Some(Publication::Confidential));
        assert_eq!(back.publication_by.as_deref(), Some("@anna:x"));
    }

    #[test]
    fn a_state_from_before_link_requests_has_none() {
        let st: State = serde_json::from_str(r#"{"plenums":{}}"#).unwrap();
        assert!(st.link_requests.is_empty());
        let d = LinkDelivery {
            person: "u-anna".into(),
            username: "anna".into(),
            account: "@anna:matrix.org".into(),
            requested_at: "2026-10-31T10:00:00Z".parse().unwrap(),
            handled_at: "2026-10-31T10:00:05Z".parse().unwrap(),
            outcome: LinkOutcome::GaveUp,
            room: Some("!dm:x".into()),
            introduced: true,
            attempts: 2,
            reason: Some("not joined within 30 minutes".into()),
            repeats: 0,
        };
        let json = serde_json::to_string(&d).unwrap();
        assert!(json.contains(r#""outcome":"gave_up""#), "{json}");
        assert_eq!(serde_json::from_str::<LinkDelivery>(&json).unwrap(), d);
    }

    #[test]
    fn a_transaction_id_is_made_once() {
        let mut s = Step::default();
        assert_eq!(s.txn(), s.txn());
    }

    #[test]
    fn pruning_keeps_open_meetings() {
        let mut p = PlenumState::default();
        let start = chrono::NaiveDate::from_ymd_opt(2020, 1, 1).unwrap();
        for i in 0..(KEEP_MEETINGS + 5) {
            let day = start + chrono::Duration::days(i as i64);
            let mut m = Meeting {
                start: Some(MeetingKey(day.and_hms_opt(19, 0, 0).unwrap())),
                ..Default::default()
            };
            m.close.done = i != 0; // the oldest is still open
            p.meetings.insert(
                MeetingKey(day.and_hms_opt(19, 0, 0).unwrap()).to_string(),
                m,
            );
        }
        p.prune();
        assert_eq!(p.meetings.len(), KEEP_MEETINGS);
        assert!(
            !p.meetings.values().next().unwrap().close.done,
            "the open one stays"
        );
    }
}
