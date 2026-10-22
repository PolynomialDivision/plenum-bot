//! What the bot remembers between restarts (`store/state.json`).
//!
//! Every step of a meeting is recorded as soon as it is done, and a message's transaction
//! ID before it is sent: a restart in the middle repeats nothing that already happened
//! (the homeserver drops a resend with the same transaction ID) and skips nothing.

use std::{collections::BTreeMap, path::Path};

use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{calendar::Occurrence, members::RoomMembers, schedule::MeetingKey};

/// Meetings kept per plenum; older ones are dropped (their protocols stay in Outline).
const KEEP_MEETINGS: usize = 60;

/// Decisions the overview shows (the full list is the decision document).
pub const KEEP_DECISIONS: usize = 10;

#[derive(Serialize, Deserialize, Default, Debug)]
pub struct State {
    #[serde(default)]
    pub plenums: BTreeMap<String, PlenumState>,
    /// Per room ID: whom the bot invited there (rooms of plenums with a group).
    #[serde(default)]
    pub rooms: BTreeMap<String, RoomMembers>,
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
    /// By the calendar occurrence's ID.
    #[serde(default)]
    pub meetings: BTreeMap<String, Meeting>,
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
        if self.decisions.len() > KEEP_DECISIONS {
            self.decisions
                .drain(..self.decisions.len() - KEEP_DECISIONS);
        }
    }
}

impl State {
    pub async fn load(path: &Path) -> Result<Self> {
        mxbot_common::persist::load_json_or_default(path).await
    }

    pub async fn save(&self, path: &Path) -> Result<()> {
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
