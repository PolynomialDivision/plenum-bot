//! What the bot remembers between restarts (`store/state.json`).
//!
//! Every step of a meeting is recorded as soon as it is done, and a message's transaction
//! ID before it is sent: a restart in the middle repeats nothing that already happened
//! (the homeserver drops a resend with the same transaction ID) and skips nothing.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};

use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::schedule::MeetingKey;

/// Meetings kept per plenum; older ones are dropped (their protocols stay in Outline).
const KEEP_MEETINGS: usize = 60;

#[derive(Serialize, Deserialize, Default, Debug)]
pub struct State {
    #[serde(default)]
    pub plenums: BTreeMap<String, PlenumState>,
}

#[derive(Serialize, Deserialize, Default, Debug)]
pub struct PlenumState {
    #[serde(default)]
    pub collection_id: Option<String>,
    #[serde(default)]
    pub template_doc_id: Option<String>,
    #[serde(default)]
    pub decisions_doc_id: Option<String>,
    /// Dates added with `!plenum extra`.
    #[serde(default)]
    pub extra: BTreeSet<MeetingKey>,
    /// Dates cancelled with `!plenum absagen`.
    #[serde(default)]
    pub cancelled: BTreeSet<MeetingKey>,
    /// Agenda items sent before the next protocol existed; they go into it.
    #[serde(default)]
    pub pending_tops: Vec<Top>,
    #[serde(default)]
    pub meetings: BTreeMap<MeetingKey, Meeting>,
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
    /// Someone said `!protokoll fertig`: close at the next tick, whatever the time.
    #[serde(default)]
    pub requested: bool,
    #[serde(default)]
    pub decisions_saved: bool,
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
    /// Forget the oldest finished meetings beyond [`KEEP_MEETINGS`], and dates that passed.
    pub fn prune(&mut self, today: chrono::NaiveDate) {
        while self.meetings.len() > KEEP_MEETINGS {
            let Some(oldest) = self
                .meetings
                .iter()
                .find(|(_, m)| m.close.done || m.skipped)
                .map(|(k, _)| *k)
            else {
                break;
            };
            self.meetings.remove(&oldest);
        }
        let old = today - chrono::Duration::days(400);
        self.extra.retain(|k| k.date() >= old);
        self.cancelled.retain(|k| k.date() >= old);
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
        let p = st.plenums.entry("haus".into()).or_default();
        let key: MeetingKey = "2026-11-01 19:00".parse().unwrap();
        let mut m = Meeting {
            doc_id: "d1".into(),
            created: true,
            ..Default::default()
        };
        let txn = m.announce.txn();
        p.meetings.insert(key, m);
        p.cancelled.insert("2026-12-06 19:00".parse().unwrap());
        st.save(&path).await.unwrap();

        let back = State::load(&path).await.unwrap();
        let p = &back.plenums["haus"];
        assert_eq!(p.meetings[&key].announce.txn.as_deref(), Some(txn.as_str()));
        assert!(p.meetings[&key].created);
        assert_eq!(p.cancelled.len(), 1);
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
            let mut m = Meeting::default();
            m.close.done = i != 0; // the oldest is still open
            p.meetings
                .insert(MeetingKey(day.and_hms_opt(19, 0, 0).unwrap()), m);
        }
        p.prune(start);
        assert_eq!(p.meetings.len(), KEEP_MEETINGS);
        assert!(
            !p.meetings.values().next().unwrap().close.done,
            "the open one stays"
        );
    }
}
