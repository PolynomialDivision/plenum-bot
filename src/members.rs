//! The members of a plenum's room follow its directory group: who is in the group is invited
//! once, who is in the room without being in it is removed (all of them when the group is
//! empty). Pure planning here; the engine reads the room and the directory (who is in the
//! group and in its `require_group`) and carries the plan out.
//!
//! Never removed: the bot itself, the accounts in `[members] keep` (the house's bots, the
//! provisioner's room admins) and anyone with power level 100 in the room (its admins). Who
//! leaves a room on their own is not invited again while they stay in the group (the bot
//! remembers whom it invited); leaving the group and coming back invites them again.

use std::collections::{BTreeMap, BTreeSet, HashSet};

use chrono::{DateTime, Duration, Utc};
use mxbot_common::matrix_sdk::ruma::{OwnedUserId, UserId};
use serde::{Deserialize, Serialize};

/// Someone in a room, joined or invited.
#[derive(Clone, Debug, PartialEq)]
pub struct Member {
    pub id: OwnedUserId,
    /// Power level 100 or more: a room admin, never removed.
    pub admin: bool,
}

/// What the bot remembers per room (`state.json`).
#[derive(Serialize, Deserialize, Default, Debug, Clone, PartialEq)]
pub struct RoomMembers {
    /// Invited by the bot (not invited again after they leave on their own).
    #[serde(default)]
    pub invited: BTreeSet<String>,
    /// Invitations that failed (an account that does not exist yet), tried again a day later.
    #[serde(default)]
    pub failed: BTreeMap<String, DateTime<Utc>>,
}

/// How long a failed invitation waits before it is tried again.
pub const RETRY_FAILED: Duration = Duration::hours(24);

#[derive(Debug, Default, PartialEq)]
pub struct Plan {
    pub invite: Vec<OwnedUserId>,
    pub remove: Vec<OwnedUserId>,
}

/// `wanted`: the group's accounts; `current`: who is in the room now (joined or invited).
pub fn plan(
    wanted: &BTreeSet<OwnedUserId>,
    current: &[Member],
    keep: &HashSet<OwnedUserId>,
    me: &UserId,
    remembered: &RoomMembers,
    now: DateTime<Utc>,
) -> Plan {
    let here: HashSet<&str> = current.iter().map(|m| m.id.as_str()).collect();
    let invite = wanted
        .iter()
        .filter(|id| !here.contains(id.as_str()) && id.as_str() != me.as_str())
        .filter(|id| !remembered.invited.contains(id.as_str()))
        .filter(|id| {
            remembered
                .failed
                .get(id.as_str())
                .is_none_or(|at| now - *at >= RETRY_FAILED)
        })
        .cloned()
        .collect();
    let mut remove: Vec<OwnedUserId> = current
        .iter()
        .filter(|m| !m.admin && m.id != me && !keep.contains(&m.id) && !wanted.contains(&m.id))
        .map(|m| m.id.clone())
        .collect();
    remove.sort();
    Plan { invite, remove }
}

/// Forget whom the group no longer has: who comes back is invited again.
pub fn forget_others(remembered: &mut RoomMembers, wanted: &BTreeSet<OwnedUserId>) {
    remembered
        .invited
        .retain(|id| wanted.iter().any(|w| w.as_str() == id));
    remembered
        .failed
        .retain(|id, _| wanted.iter().any(|w| w.as_str() == id));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(s: &str) -> OwnedUserId {
        UserId::parse(s).unwrap()
    }

    fn set(list: &[&str]) -> BTreeSet<OwnedUserId> {
        list.iter().map(|s| id(s)).collect()
    }

    fn member(s: &str, admin: bool) -> Member {
        Member { id: id(s), admin }
    }

    fn now() -> DateTime<Utc> {
        "2026-10-22T10:00:00Z".parse().unwrap()
    }

    #[test]
    fn group_members_come_in_and_others_go_out() {
        let wanted = set(&["@anna:h", "@ben:h"]);
        let current = [
            member("@anna:h", false),
            member("@eve:h", false),     // not in the group
            member("@admin:h", true),    // a room admin
            member("@cal-bot:h", false), // kept
            member("@plenum-bot:h", false),
        ];
        let keep = HashSet::from([id("@cal-bot:h")]);
        let p = plan(
            &wanted,
            &current,
            &keep,
            &id("@plenum-bot:h"),
            &RoomMembers::default(),
            now(),
        );
        assert_eq!(p.invite, vec![id("@ben:h")]);
        assert_eq!(p.remove, vec![id("@eve:h")]);
    }

    #[test]
    fn who_left_on_their_own_is_not_invited_again_until_the_group_changes() {
        let wanted = set(&["@anna:h"]);
        let mut remembered = RoomMembers::default();
        remembered.invited.insert("@anna:h".into());
        let p = plan(
            &wanted,
            &[],
            &HashSet::new(),
            &id("@bot:h"),
            &remembered,
            now(),
        );
        assert!(p.invite.is_empty());
        // out of the group and back: invited again
        forget_others(&mut remembered, &set(&[]));
        assert!(remembered.invited.is_empty());
        let p = plan(
            &wanted,
            &[],
            &HashSet::new(),
            &id("@bot:h"),
            &remembered,
            now(),
        );
        assert_eq!(p.invite, vec![id("@anna:h")]);
    }

    #[test]
    fn a_failed_invitation_waits_a_day() {
        let wanted = set(&["@new:h"]);
        let mut remembered = RoomMembers::default();
        remembered
            .failed
            .insert("@new:h".into(), now() - Duration::hours(2));
        assert!(plan(
            &wanted,
            &[],
            &HashSet::new(),
            &id("@bot:h"),
            &remembered,
            now()
        )
        .invite
        .is_empty());
        remembered
            .failed
            .insert("@new:h".into(), now() - Duration::hours(25));
        assert_eq!(
            plan(
                &wanted,
                &[],
                &HashSet::new(),
                &id("@bot:h"),
                &remembered,
                now()
            )
            .invite,
            vec![id("@new:h")]
        );
    }
}
