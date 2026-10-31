//! Link codes for the requests the identity provider's flow writes.
//!
//! In the flow `house-matrix-accounts` a logged-in person may name an account on another
//! server without a code: the flow writes a link request on them
//! ([`mxbot_common::directory::LinkRequest`]) and tells them the bot is inviting that account.
//! Every [`ROUND`] the bot reads the requests; for each fresh one it has not taken up yet it
//! opens a direct chat with the account (an existing private one, else a new encrypted one
//! inviting it), says which house login asked, and once the account has joined sends the code
//! ([`mxbot_common::link::code`] for the person's login and that account, current slot): the
//! same code `!link` gives ([`crate::linking`]), so the account itself proves it reads there.
//!
//! Who may ask: the directory reads only the requests of the active people in the groups
//! whose rooms the bot manages (its plenums' groups and the groups these require); a guest or
//! a former resident whose user is still active asks for nothing.
//!
//! The account may be anyone's: whoever asks names it. So what the bot writes there says only
//! the asking person's login (which they cannot change), as code, never the name they show
//! (which they edit themselves: it would carry their text, links or lines into a message the
//! bot sends to a stranger).
//!
//! Not served: a request older than a slot (or far in the future), an account on the house's
//! server (it needs no link), one linked to the person already, one another active person
//! claims (from the last full read of the directory; none yet: wait). At most
//! [`PER_PERSON_PER_DAY`] deliveries per person, [`PER_ACCOUNT_PER_DAY`] per account (whoever
//! asks: nobody's account is written to over and over) and [`PER_HOUR`] in all; more are
//! skipped (logged, counted in the status file), not tried later. An account that has not
//! joined within a slot, a request withdrawn or replaced meanwhile, or Matrix failing
//! [`MAX_ATTEMPTS`] times: given up, never retried (the person asks again).
//!
//! Every request taken up is remembered in the state ([`crate::state::LinkDelivery`], a day
//! long, saved before the invitation goes out): a restart neither invites nor sends twice.
//! A request that is skipped or refused is remembered as one record per person, the newest
//! (with how many it stands for): a script asking every minute adds no record a minute.
//! The messages carry transaction IDs made from the request's key, so a send repeated after a
//! crash is dropped by the homeserver; the chat is remembered as soon as it is made (only a
//! crash between making it and saving it would make a second one). The code is in the message
//! only, never in the log or the state.
//!
//! The decisions are pure functions ([`eligibility`], [`within_limits`]) and [`round`] runs
//! against [`LinkChat`], so both are tested without a homeserver; main.rs has the Matrix side.

use std::{
    collections::{BTreeMap, HashSet},
    path::Path,
    time::{Duration, SystemTime},
};

use chrono::{DateTime, Utc};
use mxbot_common::{
    directory::{LinkRequest, People},
    link,
    matrix_sdk::ruma::{OwnedRoomId, RoomId, UserId},
};
use tokio::sync::Mutex;
use tracing::{info, warn};

use crate::state::{LinkDelivery, LinkOutcome, State};

/// How often the requests are read.
pub const ROUND: Duration = Duration::from_secs(20);

/// Deliveries one person gets per 24 hours.
pub const PER_PERSON_PER_DAY: usize = 5;

/// Deliveries to one account per 24 hours, whoever asks.
pub const PER_ACCOUNT_PER_DAY: usize = 3;

/// Deliveries per hour in all.
pub const PER_HOUR: usize = 30;

/// Failed attempts at opening the chat or sending the code before a delivery is given up.
pub const MAX_ATTEMPTS: u32 = 5;

/// How long a delivery is remembered (and counted for the limits).
const KEEP: Duration = Duration::from_secs(24 * 3600);

/// How long the account has to join: a slot, as long as a code holds at least.
const JOIN_WITHIN: Duration = Duration::from_secs(link::SLOT_SECONDS);

/// The Matrix side of a delivery.
pub trait LinkChat {
    /// The bot's private chat with `account`: an existing one where both are joined, nobody
    /// else, encrypted; else a new encrypted direct chat inviting it.
    async fn open(&self, account: &UserId) -> anyhow::Result<OwnedRoomId>;

    /// Whether `room` is the bot's private chat with `account` and `account` has joined
    /// (asked of the server).
    async fn joined(&self, room: &RoomId, account: &UserId) -> anyhow::Result<bool>;

    /// `markdown` into `room`, once (`key` makes its transaction ID).
    async fn send(&self, room: &RoomId, markdown: &str, key: &str) -> anyhow::Result<()>;
}

/// What a delivery needs besides the request.
pub struct Desk {
    /// The key shared with the identity provider (`MATRIX_LINK_KEY`).
    pub key: Vec<u8>,
    /// The house's server: its accounts need no link.
    pub house_server: String,
    /// The flow where the code is entered (`[members] choose_url`).
    pub choose_url: Option<String>,
}

/// Whether a request is served.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Eligibility {
    Deliver,
    /// Too old or too far ahead: left alone, not remembered.
    NotFresh,
    /// No full read of the directory to check the account against yet: next round.
    Wait,
    /// Never served (remembered with the reason).
    Refused(&'static str),
}

/// Whether `request` is served now: fresh, not on the house's server, not linked to the person
/// already, and claimed by no other active person in `people` (the last full read; `None`:
/// wait for one).
pub fn eligibility(
    request: &LinkRequest,
    house_server: &str,
    people: Option<&People>,
    now: SystemTime,
) -> Eligibility {
    if !request.is_fresh(now) {
        return Eligibility::NotFresh;
    }
    if request.account.server_name().as_str() == house_server {
        return Eligibility::Refused("an account on the house's server needs no link");
    }
    if request.linked.contains(&request.account) {
        return Eligibility::Refused("the person has linked it already");
    }
    let Some(people) = people else {
        return Eligibility::Wait;
    };
    if people
        .claimants(&request.account)
        .any(|p| p.id != request.person)
    {
        return Eligibility::Refused("another active person claims this account");
    }
    Eligibility::Deliver
}

/// Whether one more delivery for `person` to `account` is within the limits:
/// [`PER_PERSON_PER_DAY`] for them, [`PER_ACCOUNT_PER_DAY`] to the account (from anyone; the
/// account compared without case), [`PER_HOUR`] in all. A delivery is one the bot invited for
/// (pending, sent or given up); skipped and refused requests do not count.
pub fn within_limits(
    handled: &BTreeMap<String, LinkDelivery>,
    person: &str,
    account: &str,
    now: DateTime<Utc>,
) -> Result<(), &'static str> {
    let delivered = |within: Duration| {
        handled.values().filter(move |d| {
            matches!(
                d.outcome,
                LinkOutcome::Pending | LinkOutcome::Sent | LinkOutcome::GaveUp
            ) && age(d.handled_at, now) < within
        })
    };
    if delivered(KEEP).filter(|d| d.person == person).count() >= PER_PERSON_PER_DAY {
        return Err("this person's deliveries of the day are used up");
    }
    if delivered(KEEP)
        .filter(|d| d.account.eq_ignore_ascii_case(account))
        .count()
        >= PER_ACCOUNT_PER_DAY
    {
        return Err("this account's deliveries of the day are used up");
    }
    if delivered(Duration::from_secs(3600)).count() >= PER_HOUR {
        return Err("the deliveries of the hour are used up");
    }
    Ok(())
}

fn age(at: DateTime<Utc>, now: DateTime<Utc>) -> Duration {
    (now - at).to_std().unwrap_or_default()
}

/// A login or a Matrix ID as inline code: shown as it is, never formatted, linked or broken
/// into lines (a backtick, which would end the code, and control characters such as line
/// breaks are left out; neither is in a login or an ID the directory gives).
fn code(text: &str) -> String {
    let clean: String = text
        .chars()
        .filter(|c| *c != '`' && !c.is_control())
        .collect();
    format!("`{clean}`")
}

/// The first message in the chat: which house login asked, for this account, and that the code
/// follows. Only the login (as code): the person cannot change it, unlike the name they show,
/// and the account may be a stranger's.
pub fn invitation_text(d: &LinkDelivery) -> String {
    let login = code(&d.username);
    let account = code(&d.account);
    format!(
        "🔗 Die Haus-Anmeldung {login} möchte dieses Matrix-Konto {account} mit dem Haus \
         verknüpfen. Nimm die Einladung an, dann bekommst du den Code. Warst du das nicht? \
         Dann ignoriere das hier.\n\n\
         _The house login {login} wants to link this Matrix account to the house. Accept the \
         invitation and you get the code. Not you? Then ignore this._"
    )
}

/// The message with the code (the login as code, as in [`invitation_text`]).
pub fn code_text(username: &str, code_value: &str, choose_url: Option<&str>) -> String {
    let login = code(username);
    let url = choose_url.map_or(String::new(), |u| format!("\n\n{u}"));
    format!(
        "Dein Code: **{code_value}** — trag ihn in den Einstellungen bei 'Matrix-Konten' ein \
         (gilt 30–60 Minuten). Gib ihn niemandem weiter: er verknüpft dieses Konto mit {login}.\
         {url}\n\n\
         _Your code: enter it in the settings under 'Matrix accounts' (valid for 30–60 \
         minutes). Do not give it to anyone: it links this account to {login}._"
    )
}

/// What a round did, for the log and the status file.
#[derive(Debug, Default)]
pub struct Report {
    /// Chats opened (invitations sent) and codes sent.
    pub opened: usize,
    pub sent: usize,
    /// Taken up this round: refused, skipped over a limit, given up.
    pub refused: usize,
    pub skipped: usize,
    pub gave_up: usize,
    /// Requests that wait for a full read of the directory.
    pub waiting: usize,
    /// Matrix or the state failed (tried again next round).
    pub errors: Vec<String>,
    /// Deliveries waiting for their account (or for their chat), and since when the oldest.
    pub pending: u64,
    pub oldest: Option<DateTime<Utc>>,
    /// Remembered (the last day): skipped over a limit (every request, also those a person's
    /// one record stands for), given up.
    pub skipped_day: usize,
    pub gave_up_day: usize,
}

impl Report {
    /// Anything happened this round (for ok vs idle).
    pub fn did_something(&self) -> bool {
        self.opened + self.sent + self.refused + self.skipped + self.gave_up > 0
    }
}

/// What a delivery's Matrix step came to.
enum Step {
    /// Its chat is open (a new room ID to remember).
    Opened(OwnedRoomId),
    /// The first message (who asked) is in it.
    Introduced,
    Sent,
    Waiting,
    Failed(String),
    GaveUp(String),
}

/// One round: forget what is a day old; with `requests` (None: the read failed, nothing new
/// and nothing withdrawn), give up the pending deliveries whose request is gone or replaced and
/// take up the new ones ([`eligibility`], [`within_limits`]), saved before anything is sent;
/// then per pending delivery open its chat, or send the code once its account has joined, or
/// give it up after a slot. The state's lock is not held while Matrix is asked.
#[allow(clippy::too_many_arguments)]
pub async fn round<C: LinkChat>(
    chat: &C,
    desk: &Desk,
    state: &Mutex<State>,
    state_path: &Path,
    requests: Option<&[LinkRequest]>,
    people: Option<&People>,
    now: DateTime<Utc>,
) -> Report {
    let mut report = Report::default();
    let work: Vec<(String, LinkDelivery)> = {
        let mut st = state.lock().await;
        let before = st.link_requests.len();
        st.link_requests
            .retain(|_, d| d.outcome == LinkOutcome::Pending || age(d.handled_at, now) < KEEP);
        let mut changed = st.link_requests.len() != before;
        if let Some(requests) = requests {
            changed |= take_up(
                &mut st.link_requests,
                requests,
                desk,
                people,
                now,
                &mut report,
            );
        }
        if changed {
            if let Err(e) = st.save(state_path).await {
                // nothing goes out that a restart would not know about
                report.errors.push(format!("saving the state: {e:#}"));
                summarize(&st.link_requests, &mut report);
                return report;
            }
        }
        st.link_requests
            .iter()
            .filter(|(_, d)| d.outcome == LinkOutcome::Pending)
            .map(|(k, d)| (k.clone(), d.clone()))
            .collect()
    };

    let mut steps: Vec<(String, Step)> = Vec::new();
    for (key, d) in work {
        for step in deliver(chat, desk, &key, &d, now).await {
            steps.push((key.clone(), step));
        }
    }

    let mut st = state.lock().await;
    let mut changed = false;
    for (key, step) in steps {
        let Some(d) = st
            .link_requests
            .get_mut(&key)
            .filter(|d| d.outcome == LinkOutcome::Pending)
        else {
            continue;
        };
        match step {
            Step::Opened(room) => {
                info!(person = %d.username, account = %d.account, room = %room,
                    "Link request: the account is invited, the code follows when it joins");
                d.room = Some(room.to_string());
                report.opened += 1;
            }
            Step::Introduced => d.introduced = true,
            Step::Sent => {
                // the code itself is in the message only
                info!(person = %d.username, account = %d.account, "Link request: code sent");
                d.outcome = LinkOutcome::Sent;
                report.sent += 1;
            }
            Step::Waiting => continue,
            Step::Failed(why) => {
                d.attempts += 1;
                if d.attempts >= MAX_ATTEMPTS {
                    warn!(person = %d.username, account = %d.account,
                        "Link request given up after {} failed attempts: {why}", d.attempts);
                    d.outcome = LinkOutcome::GaveUp;
                    d.reason = Some(format!("Matrix failed {} times: {why}", d.attempts));
                    report.gave_up += 1;
                } else {
                    warn!(person = %d.username, account = %d.account,
                        "Link request: {why} (tried again next round)");
                    report.errors.push(why);
                }
            }
            Step::GaveUp(why) => {
                warn!(person = %d.username, account = %d.account, "Link request given up: {why}");
                d.outcome = LinkOutcome::GaveUp;
                d.reason = Some(why);
                report.gave_up += 1;
            }
        }
        changed = true;
    }
    if changed {
        if let Err(e) = st.save(state_path).await {
            report.errors.push(format!("saving the state: {e:#}"));
        }
    }
    summarize(&st.link_requests, &mut report);
    report
}

/// The round's bookkeeping under the state's lock: pending deliveries whose request is gone
/// given up, new requests recorded (refused, skipped or pending). A refused or skipped request
/// replaces the person's earlier refused or skipped record whose request is gone (one per
/// person, counting what it replaced): the request it was for cannot come back (a person has
/// one, a new one replaces it), so nothing is taken up twice, and the state does not grow
/// with every request a script sends. True when anything changed.
fn take_up(
    handled: &mut BTreeMap<String, LinkDelivery>,
    requests: &[LinkRequest],
    desk: &Desk,
    people: Option<&People>,
    now: DateTime<Utc>,
    report: &mut Report,
) -> bool {
    let mut changed = false;
    // withdrawn, replaced by a newer one, or linked meanwhile (the flow removes it then)
    let current: HashSet<String> = requests.iter().map(LinkRequest::key).collect();
    for (key, d) in handled.iter_mut() {
        if d.outcome == LinkOutcome::Pending && !current.contains(key) {
            info!(person = %d.username, account = %d.account,
                "Link request withdrawn or replaced: no code is sent");
            d.outcome = LinkOutcome::GaveUp;
            d.reason = Some("the request was withdrawn or replaced".into());
            report.gave_up += 1;
            changed = true;
        }
    }
    let mut fresh: Vec<&LinkRequest> = requests.iter().collect();
    fresh.sort_by_key(|r| r.at);
    for r in fresh {
        let key = r.key();
        if handled.contains_key(&key) {
            continue;
        }
        let record = |outcome: LinkOutcome, reason: Option<&str>| LinkDelivery {
            person: r.person.clone(),
            username: r.username.clone(),
            account: r.account.to_string(),
            requested_at: r.at,
            handled_at: now,
            outcome,
            room: None,
            introduced: false,
            attempts: 0,
            reason: reason.map(str::to_owned),
            repeats: 0,
        };
        let mut delivery = match eligibility(r, &desk.house_server, people, SystemTime::from(now)) {
            Eligibility::NotFresh => continue,
            Eligibility::Wait => {
                report.waiting += 1;
                continue;
            }
            Eligibility::Refused(why) => {
                info!(person = %r.username, account = %r.account, "Link request not served: {why}");
                report.refused += 1;
                record(LinkOutcome::Refused, Some(why))
            }
            Eligibility::Deliver => {
                match within_limits(handled, &r.person, r.account.as_str(), now) {
                    Err(why) => {
                        warn!(person = %r.username, account = %r.account, "Link request skipped: {why}");
                        report.skipped += 1;
                        record(LinkOutcome::Skipped, Some(why))
                    }
                    Ok(()) => record(LinkOutcome::Pending, None),
                }
            }
        };
        // one refused or skipped record per person: the newest stands for the earlier ones
        if matches!(
            delivery.outcome,
            LinkOutcome::Skipped | LinkOutcome::Refused
        ) {
            let earlier: Vec<String> = handled
                .iter()
                .filter(|(k, d)| {
                    d.person == r.person
                        && matches!(d.outcome, LinkOutcome::Skipped | LinkOutcome::Refused)
                        && !current.contains(*k)
                })
                .map(|(k, _)| k.clone())
                .collect();
            for k in earlier {
                if let Some(d) = handled.remove(&k) {
                    delivery.repeats += 1 + d.repeats;
                }
            }
        }
        handled.insert(key, delivery);
        changed = true;
    }
    changed
}

/// The Matrix steps of one pending delivery this round.
async fn deliver<C: LinkChat>(
    chat: &C,
    desk: &Desk,
    key: &str,
    d: &LinkDelivery,
    now: DateTime<Utc>,
) -> Vec<Step> {
    if age(d.handled_at, now) >= JOIN_WITHIN {
        let why = if d.room.is_some() {
            "the account did not join within 30 minutes"
        } else {
            "the chat could not be opened within 30 minutes"
        };
        return vec![Step::GaveUp(why.into())];
    }
    let Ok(account) = UserId::parse(&d.account) else {
        return vec![Step::GaveUp("not a Matrix ID".into())];
    };
    let mut steps = Vec::new();
    // Opened and introduced are steps of their own: a room made whose first message failed is
    // remembered, and the next round sends into it instead of making another one.
    let room = match d.room.as_deref().map(RoomId::parse) {
        Some(Ok(room)) => room,
        Some(Err(_)) => return vec![Step::GaveUp("not a room ID".into())],
        None => match chat.open(&account).await {
            Ok(room) => {
                steps.push(Step::Opened(room.clone()));
                room
            }
            Err(e) => return vec![Step::Failed(format!("opening the chat: {e:#}"))],
        },
    };
    if !d.introduced {
        let first = invitation_text(d);
        match chat
            .send(&room, &first, &format!("link-invite:{key}"))
            .await
        {
            Ok(()) => steps.push(Step::Introduced),
            Err(e) => {
                steps.push(Step::Failed(format!("sending who asked: {e:#}")));
                return steps;
            }
        }
    }
    match chat.joined(&room, &account).await {
        Ok(true) => {
            let code = link::code(
                &desk.key,
                &d.username,
                &account,
                link::slot_at(SystemTime::from(now)),
            );
            let text = code_text(&d.username, &code, desk.choose_url.as_deref());
            match chat.send(&room, &text, &format!("link-code:{key}")).await {
                Ok(()) => steps.push(Step::Sent),
                Err(e) => steps.push(Step::Failed(format!("sending the code: {e:#}"))),
            }
        }
        Ok(false) => steps.push(Step::Waiting),
        Err(e) => {
            // asked again next round; only the slot ends the wait
            info!(account = %d.account, "Link request: whether the account joined is unknown: {e:#}");
            steps.push(Step::Waiting);
        }
    }
    steps
}

/// The pending deliveries and the day's skipped and given-up ones, into the report.
fn summarize(handled: &BTreeMap<String, LinkDelivery>, report: &mut Report) {
    let pending = handled
        .values()
        .filter(|d| d.outcome == LinkOutcome::Pending);
    report.pending = pending.clone().count() as u64;
    report.oldest = pending.map(|d| d.handled_at).min();
    report.skipped_day = handled
        .values()
        .filter(|d| d.outcome == LinkOutcome::Skipped)
        .map(|d| 1 + d.repeats as usize)
        .sum();
    report.gave_up_day = handled
        .values()
        .filter(|d| d.outcome == LinkOutcome::GaveUp)
        .count();
}

#[cfg(test)]
mod tests {
    use super::*;
    use mxbot_common::{
        directory::{Choice, Link, Person},
        matrix_sdk::ruma::OwnedUserId,
    };
    use std::sync::Mutex as StdMutex;

    fn id(s: &str) -> OwnedUserId {
        UserId::parse(s).unwrap()
    }

    /// 2026-10-31 10:00:00 UTC, at the start of a slot.
    fn t0() -> DateTime<Utc> {
        let secs = (1_793_440_800 / link::SLOT_SECONDS) * link::SLOT_SECONDS;
        DateTime::from_timestamp(secs as i64, 0).unwrap()
    }

    fn secs(n: i64) -> chrono::Duration {
        chrono::Duration::seconds(n)
    }

    fn request(person: &str, account: &str, at: DateTime<Utc>) -> LinkRequest {
        LinkRequest {
            person: format!("u-{person}"),
            username: person.into(),
            account: id(account),
            at,
            linked: Vec::new(),
        }
    }

    fn person(name: &str, links: &[&str]) -> Person {
        let house = id(&format!("@{name}:house.org"));
        let links: Vec<Link> = links.iter().map(|l| Link { id: id(l) }).collect();
        Person {
            id: format!("u-{name}"),
            username: name.into(),
            name: String::new(),
            active: true,
            accounts: std::iter::once(house.clone())
                .chain(links.iter().map(|l| l.id.clone()))
                .collect(),
            groups: Default::default(),
            house: Some(house),
            links,
            choice: Choice::None,
            stay_until: None,
        }
    }

    fn desk() -> Desk {
        Desk {
            key: b"test-key".to_vec(),
            house_server: "house.org".into(),
            choose_url: Some("https://login.example.org/if/flow/house-matrix-accounts/".into()),
        }
    }

    #[test]
    fn which_requests_are_served() {
        let now = SystemTime::from(t0());
        let people = People::new(vec![
            person("anna", &[]),
            person("ben", &["@ben:matrix.org"]),
        ]);
        let anna = request("anna", "@anna:matrix.org", t0() - secs(60));
        assert_eq!(
            eligibility(&anna, "house.org", Some(&people), now),
            Eligibility::Deliver
        );
        // no full read yet: wait for one
        assert_eq!(
            eligibility(&anna, "house.org", None, now),
            Eligibility::Wait
        );
        // stale, or far ahead: left alone
        let old = request("anna", "@anna:matrix.org", t0() - secs(1800));
        assert_eq!(
            eligibility(&old, "house.org", Some(&people), now),
            Eligibility::NotFresh
        );
        let ahead = request("anna", "@anna:matrix.org", t0() + secs(301));
        assert_eq!(
            eligibility(&ahead, "house.org", Some(&people), now),
            Eligibility::NotFresh
        );
        // the house's server, linked already, someone else's
        let house = request("anna", "@anna:house.org", t0());
        assert!(matches!(
            eligibility(&house, "house.org", Some(&people), now),
            Eligibility::Refused(_)
        ));
        let mut linked = request("ben", "@ben:matrix.org", t0());
        linked.linked = vec![id("@ben:matrix.org")];
        assert!(matches!(
            eligibility(&linked, "house.org", Some(&people), now),
            Eligibility::Refused(_)
        ));
        let bens = request("anna", "@ben:matrix.org", t0());
        assert_eq!(
            eligibility(&bens, "house.org", Some(&people), now),
            Eligibility::Refused("another active person claims this account")
        );
        // claimed in the full read by the asking person only (not in the request's `linked`): served
        let own = request("ben", "@ben:matrix.org", t0());
        assert_eq!(
            eligibility(&own, "house.org", Some(&people), now),
            Eligibility::Deliver
        );
    }

    fn delivery(person: &str, outcome: LinkOutcome, at: DateTime<Utc>) -> LinkDelivery {
        LinkDelivery {
            person: format!("u-{person}"),
            username: person.into(),
            account: format!("@{person}:matrix.org"),
            requested_at: at,
            handled_at: at,
            outcome,
            room: None,
            introduced: false,
            attempts: 0,
            reason: None,
            repeats: 0,
        }
    }

    #[test]
    fn five_a_day_per_person_and_thirty_an_hour() {
        let now = t0();
        let mut handled = BTreeMap::new();
        for i in 0..PER_PERSON_PER_DAY {
            let outcome = [LinkOutcome::Pending, LinkOutcome::Sent, LinkOutcome::GaveUp][i % 3];
            let mut d = delivery("anna", outcome, now - secs(3600 * (i as i64 + 1)));
            // five different accounts: the account's own limit is not what stops her
            d.account = format!("@anna{i}:matrix.org");
            handled.insert(format!("a{i}"), d);
        }
        let to = "@new:matrix.org";
        assert!(within_limits(&handled, "u-anna", to, now).is_err());
        assert!(within_limits(&handled, "u-ben", to, now).is_ok());
        // a day after the oldest (five hours ago) it no longer counts
        assert!(within_limits(&handled, "u-anna", to, now + secs(19 * 3600 - 1)).is_err());
        assert!(within_limits(&handled, "u-anna", to, now + secs(19 * 3600)).is_ok());
        // skipped and refused ones do not count
        let mut few = BTreeMap::new();
        for i in 0..10 {
            few.insert(format!("s{i}"), delivery("anna", LinkOutcome::Skipped, now));
            few.insert(format!("r{i}"), delivery("anna", LinkOutcome::Refused, now));
        }
        assert!(within_limits(&few, "u-anna", "@anna:matrix.org", now).is_ok());
        // thirty in the last hour, from anyone
        let mut busy = BTreeMap::new();
        for i in 0..PER_HOUR {
            busy.insert(
                format!("p{i}"),
                delivery(&format!("p{i}"), LinkOutcome::Sent, now - secs(60)),
            );
        }
        assert_eq!(
            within_limits(&busy, "u-anna", to, now),
            Err("the deliveries of the hour are used up")
        );
        assert!(within_limits(&busy, "u-anna", to, now + secs(3600)).is_ok());
    }

    #[test]
    fn three_a_day_to_one_account_whoever_asks() {
        let now = t0();
        let mut handled = BTreeMap::new();
        // three people ask for the same stranger's account
        for (i, who) in ["anna", "ben", "cara"].into_iter().enumerate() {
            let mut d = delivery(who, LinkOutcome::Sent, now - secs(600 * (i as i64 + 1)));
            d.account = "@victim:matrix.org".into();
            handled.insert(format!("k{i}"), d);
        }
        assert_eq!(
            within_limits(&handled, "u-dora", "@victim:matrix.org", now),
            Err("this account's deliveries of the day are used up")
        );
        // the same account in other letters is the same account
        assert!(within_limits(&handled, "u-dora", "@Victim:matrix.org", now).is_err());
        assert!(within_limits(&handled, "u-dora", "@dora:matrix.org", now).is_ok());
        // a day after the oldest of them
        assert!(within_limits(
            &handled,
            "u-dora",
            "@victim:matrix.org",
            now + secs(86_400 - 1800)
        )
        .is_ok());
    }

    #[test]
    fn the_messages_name_only_the_login_and_hold_the_code() {
        let d = delivery("anna_b", LinkOutcome::Pending, t0());
        let text = invitation_text(&d);
        assert!(
            text.starts_with(
                "🔗 Die Haus-Anmeldung `anna_b` möchte dieses Matrix-Konto \
                 `@anna_b:matrix.org` mit dem Haus verknüpfen."
            ),
            "{text}"
        );
        assert!(
            text.contains("_The house login `anna_b` wants to link"),
            "{text}"
        );
        assert!(text.contains("Warst du das nicht?") && text.contains("Not you?"));
        // whatever a login would carry stays one piece of code on one line
        let mut odd = d.clone();
        odd.username = "x`\n[click](https://evil.example)".into();
        let text = invitation_text(&odd);
        assert!(
            text.starts_with("🔗 Die Haus-Anmeldung `x[click](https://evil.example)` möchte"),
            "{text}"
        );
        assert_eq!(text.matches('\n').count(), 2, "{text}");
        let text = code_text("anna", "W710-C4F3", Some("https://login.x/flow/"));
        assert!(
            text.starts_with("Dein Code: **W710-C4F3** — trag ihn"),
            "{text}"
        );
        assert!(text.contains("er verknüpft dieses Konto mit `anna`."));
        assert!(text.contains("https://login.x/flow/") && text.contains("Your code"));
        assert!(!code_text("anna", "X", None).contains("https://"));
    }

    /// A homeserver in memory: who joined which room, what was sent.
    #[derive(Default)]
    struct FakeChat {
        opened: StdMutex<Vec<OwnedUserId>>,
        sent: StdMutex<Vec<(String, String, String)>>,
        joined: StdMutex<HashSet<OwnedUserId>>,
        fail_open: StdMutex<bool>,
        fail_send: StdMutex<bool>,
    }

    impl LinkChat for FakeChat {
        async fn open(&self, account: &UserId) -> anyhow::Result<OwnedRoomId> {
            if *self.fail_open.lock().unwrap() {
                anyhow::bail!("M_FORBIDDEN");
            }
            self.opened.lock().unwrap().push(account.to_owned());
            Ok(RoomId::parse(format!("!dm-{}:house.org", account.localpart())).unwrap())
        }

        async fn joined(&self, _room: &RoomId, account: &UserId) -> anyhow::Result<bool> {
            Ok(self.joined.lock().unwrap().contains(account))
        }

        async fn send(&self, room: &RoomId, markdown: &str, key: &str) -> anyhow::Result<()> {
            if *self.fail_send.lock().unwrap() {
                anyhow::bail!("M_LIMIT_EXCEEDED");
            }
            self.sent
                .lock()
                .unwrap()
                .push((room.to_string(), markdown.into(), key.into()));
            Ok(())
        }
    }

    struct Test {
        chat: FakeChat,
        state: Mutex<State>,
        dir: tempfile::TempDir,
        people: People,
    }

    impl Test {
        fn new() -> Self {
            Self {
                chat: FakeChat::default(),
                state: Mutex::new(State::default()),
                dir: tempfile::tempdir().unwrap(),
                people: People::new(vec![person("anna", &[]), person("ben", &[])]),
            }
        }

        fn path(&self) -> std::path::PathBuf {
            self.dir.path().join("state.json")
        }

        async fn round(&self, requests: Option<&[LinkRequest]>, now: DateTime<Utc>) -> Report {
            round(
                &self.chat,
                &desk(),
                &self.state,
                &self.path(),
                requests,
                Some(&self.people),
                now,
            )
            .await
        }
    }

    #[tokio::test]
    async fn invited_then_the_code_once_joined_and_never_twice() {
        let t = Test::new();
        let requests = [request("anna", "@anna:matrix.org", t0())];
        let r = t.round(Some(&requests), t0() + secs(5)).await;
        assert_eq!((r.opened, r.sent, r.pending), (1, 0, 1));
        assert_eq!(*t.chat.opened.lock().unwrap(), [id("@anna:matrix.org")]);
        {
            let sent = t.chat.sent.lock().unwrap();
            assert_eq!(sent.len(), 1);
            assert!(sent[0].1.contains("Haus-Anmeldung `anna`"), "{}", sent[0].1);
            assert!(sent[0].2.starts_with("link-invite:"));
        }
        // not joined yet: nothing more
        let r = t.round(Some(&requests), t0() + secs(25)).await;
        assert!(!r.did_something() && r.pending == 1, "{r:?}");
        assert_eq!(t.chat.sent.lock().unwrap().len(), 1);
        // joined: the code of the current slot, for her login and this account
        t.chat.joined.lock().unwrap().insert(id("@anna:matrix.org"));
        let r = t.round(Some(&requests), t0() + secs(45)).await;
        assert_eq!((r.sent, r.pending), (1, 0));
        {
            let sent = t.chat.sent.lock().unwrap();
            let code = link::code(
                b"test-key",
                "anna",
                &id("@anna:matrix.org"),
                link::slot_at(SystemTime::from(t0())),
            );
            assert_eq!(sent.len(), 2);
            assert_eq!(sent[1].0, "!dm-anna:house.org");
            assert!(sent[1].1.contains(&format!("**{code}**")), "{}", sent[1].1);
            assert!(sent[1].2.starts_with("link-code:"));
        }
        // the same request again: done
        let r = t.round(Some(&requests), t0() + secs(65)).await;
        assert!(!r.did_something());
        assert_eq!(t.chat.opened.lock().unwrap().len(), 1);
        assert_eq!(t.chat.sent.lock().unwrap().len(), 2);
        // the code is nowhere in the state
        let saved = std::fs::read_to_string(t.path()).unwrap();
        let code = link::code(
            b"test-key",
            "anna",
            &id("@anna:matrix.org"),
            link::slot_at(SystemTime::from(t0())),
        );
        assert!(!saved.contains(&code));
        // a new request (asked again later) is a new delivery
        let again = [request("anna", "@anna:matrix.org", t0() + secs(120))];
        let r = t.round(Some(&again), t0() + secs(125)).await;
        assert_eq!(
            (r.opened, r.sent),
            (1, 1),
            "joined already: the code at once"
        );
    }

    #[tokio::test]
    async fn a_restart_sends_nothing_twice() {
        let t = Test::new();
        let requests = [request("anna", "@anna:matrix.org", t0())];
        t.round(Some(&requests), t0() + secs(5)).await;
        // a restart: the state from the file, a new chat
        let restarted = Test {
            state: Mutex::new(State::load(&t.path()).await.unwrap()),
            ..Test::new()
        };
        std::fs::copy(t.path(), restarted.path()).unwrap();
        let r = restarted.round(Some(&requests), t0() + secs(25)).await;
        assert_eq!(r.opened, 0, "{r:?}");
        assert!(restarted.chat.opened.lock().unwrap().is_empty());
        assert_eq!(r.pending, 1);
        // and the code goes into the room it opened before
        restarted
            .chat
            .joined
            .lock()
            .unwrap()
            .insert(id("@anna:matrix.org"));
        let r = restarted.round(Some(&requests), t0() + secs(45)).await;
        assert_eq!(r.sent, 1);
        let sent = restarted.chat.sent.lock().unwrap();
        assert_eq!(sent.len(), 1, "who asked was told before the restart");
        assert_eq!(sent[0].0, "!dm-anna:house.org");
    }

    #[tokio::test]
    async fn a_chat_whose_first_message_failed_is_used_again() {
        let t = Test::new();
        *t.chat.fail_send.lock().unwrap() = true;
        let requests = [request("anna", "@anna:matrix.org", t0())];
        let r = t.round(Some(&requests), t0() + secs(5)).await;
        assert_eq!((r.opened, r.errors.len(), r.pending), (1, 1, 1), "{r:?}");
        *t.chat.fail_send.lock().unwrap() = false;
        t.chat.joined.lock().unwrap().insert(id("@anna:matrix.org"));
        let r = t.round(Some(&requests), t0() + secs(25)).await;
        assert_eq!((r.opened, r.sent, r.errors.len()), (0, 1, 0), "{r:?}");
        assert_eq!(t.chat.opened.lock().unwrap().len(), 1, "no second room");
        let sent = t.chat.sent.lock().unwrap();
        // who asked first, then the code
        assert!(sent[0].2.starts_with("link-invite:") && sent[1].2.starts_with("link-code:"));
    }

    #[tokio::test]
    async fn not_joined_within_a_slot_or_withdrawn_is_given_up() {
        let t = Test::new();
        let requests = [
            request("anna", "@anna:matrix.org", t0()),
            request("ben", "@ben:matrix.org", t0()),
        ];
        t.round(Some(&requests), t0() + secs(5)).await;
        // ben's request is gone (withdrawn, replaced or linked meanwhile)
        let r = t.round(Some(&requests[..1]), t0() + secs(25)).await;
        assert_eq!((r.gave_up, r.pending), (1, 1));
        // a failed read withdraws nothing
        let r = t.round(None, t0() + secs(45)).await;
        assert_eq!((r.gave_up, r.pending), (0, 1));
        // half an hour after the invitation: given up, no code ever
        let r = t.round(Some(&requests[..1]), t0() + secs(1805)).await;
        assert_eq!((r.gave_up, r.pending, r.gave_up_day), (1, 0, 2));
        t.chat.joined.lock().unwrap().insert(id("@anna:matrix.org"));
        let r = t.round(Some(&requests[..1]), t0() + secs(1825)).await;
        assert!(!r.did_something());
        // who asked, twice; never a code
        {
            let sent = t.chat.sent.lock().unwrap();
            assert!(
                sent.len() == 2 && sent.iter().all(|s| s.2.starts_with("link-invite:")),
                "{sent:?}"
            );
        }
        let st = t.state.lock().await;
        assert!(st
            .link_requests
            .values()
            .all(|d| d.outcome == LinkOutcome::GaveUp && d.reason.is_some()));
    }

    #[tokio::test]
    async fn over_the_limit_is_skipped_once_and_counted() {
        let t = Test::new();
        for i in 0..PER_PERSON_PER_DAY as i64 {
            let account = format!("@anna{i}:matrix.org");
            let r = [request("anna", &account, t0() + secs(i * 60))];
            assert_eq!(t.round(Some(&r), t0() + secs(i * 60 + 1)).await.opened, 1);
        }
        let sixth = [request("anna", "@anna:matrix.org", t0() + secs(600))];
        let r = t.round(Some(&sixth), t0() + secs(601)).await;
        assert_eq!((r.opened, r.skipped, r.skipped_day), (0, 1, 1));
        // remembered: not skipped (or logged) again every round
        let r = t.round(Some(&sixth), t0() + secs(621)).await;
        assert_eq!((r.skipped, r.skipped_day), (0, 1));
        // ben is not held up
        let ben = [request("ben", "@ben:matrix.org", t0() + secs(600))];
        assert_eq!(t.round(Some(&ben), t0() + secs(641)).await.opened, 1);
    }

    #[tokio::test]
    async fn a_script_asking_every_minute_leaves_one_record_per_person() {
        let t = Test::new();
        for i in 0..PER_PERSON_PER_DAY as i64 {
            let account = format!("@anna{i}:matrix.org");
            let r = [request("anna", &account, t0() + secs(i * 60))];
            assert_eq!(t.round(Some(&r), t0() + secs(i * 60 + 1)).await.opened, 1);
        }
        // a thousand more requests, one a minute (each replaces the one before)
        let mut skipped = 0;
        for i in 5..1005i64 {
            let r = [request("anna", "@anna:matrix.org", t0() + secs(i * 60))];
            let report = t.round(Some(&r), t0() + secs(i * 60 + 1)).await;
            skipped += report.skipped;
            assert_eq!(report.opened, 0);
        }
        assert_eq!(skipped, 1000, "each skipped once");
        let st = t.state.lock().await;
        let anna: Vec<&LinkDelivery> = st
            .link_requests
            .values()
            .filter(|d| d.outcome == LinkOutcome::Skipped)
            .collect();
        assert_eq!(anna.len(), 1, "one record stands for them");
        assert_eq!(anna[0].repeats, 999);
        // the five deliveries, the one record: no more
        assert!(
            st.link_requests.len() <= PER_PERSON_PER_DAY + 1,
            "{}",
            st.link_requests.len()
        );
        drop(st);
        // the latest request is still known: not skipped (or logged) again
        let last = [request("anna", "@anna:matrix.org", t0() + secs(1004 * 60))];
        let r = t.round(Some(&last), t0() + secs(1004 * 60 + 30)).await;
        assert_eq!((r.skipped, r.skipped_day), (0, 1000));
    }

    #[tokio::test]
    async fn one_account_is_written_to_three_times_a_day_at_most() {
        let t = Test {
            people: People::new(vec![
                person("anna", &[]),
                person("ben", &[]),
                person("cara", &[]),
                person("dora", &[]),
            ]),
            ..Test::new()
        };
        for (i, who) in ["anna", "ben", "cara", "dora"].into_iter().enumerate() {
            let at = t0() + secs(i as i64 * 60);
            let r = [request(who, "@victim:matrix.org", at)];
            let report = t.round(Some(&r), at + secs(1)).await;
            if i < PER_ACCOUNT_PER_DAY {
                assert_eq!(report.opened, 1, "{who}");
            } else {
                assert_eq!((report.opened, report.skipped), (0, 1), "{who}");
            }
        }
        assert_eq!(t.chat.opened.lock().unwrap().len(), PER_ACCOUNT_PER_DAY);
    }

    #[tokio::test]
    async fn refused_requests_are_remembered_and_nothing_is_sent() {
        let t = Test::new();
        let requests = [
            request("anna", "@anna:house.org", t0()),
            // ben's house account is his: nobody else may link it either
            request("anna", "@ben:house.org", t0()),
        ];
        let r = t.round(Some(&requests), t0() + secs(5)).await;
        assert_eq!((r.refused, r.opened), (2, 0));
        let r = t.round(Some(&requests), t0() + secs(25)).await;
        assert_eq!(r.refused, 0);
        assert!(t.chat.opened.lock().unwrap().is_empty());
        // without a full read of the directory: nothing taken up, asked again next round
        let other = [request("ben", "@ben:matrix.org", t0())];
        let r = round(
            &t.chat,
            &desk(),
            &t.state,
            &t.path(),
            Some(&other),
            None,
            t0() + secs(30),
        )
        .await;
        assert_eq!((r.waiting, r.opened), (1, 0));
        assert_eq!(t.round(Some(&other), t0() + secs(50)).await.opened, 1);
    }

    #[tokio::test]
    async fn matrix_failing_is_tried_again_and_given_up_after_some_attempts() {
        let t = Test::new();
        *t.chat.fail_open.lock().unwrap() = true;
        let requests = [request("anna", "@anna:matrix.org", t0())];
        for i in 1..MAX_ATTEMPTS as i64 {
            let r = t.round(Some(&requests), t0() + secs(20 * i)).await;
            assert_eq!((r.errors.len(), r.pending), (1, 1), "{r:?}");
        }
        let r = t.round(Some(&requests), t0() + secs(200)).await;
        assert_eq!((r.errors.len(), r.gave_up, r.pending), (0, 1, 0), "{r:?}");
    }

    #[tokio::test]
    async fn a_day_later_the_state_forgets() {
        let t = Test::new();
        let requests = [request("anna", "@anna:matrix.org", t0())];
        t.round(Some(&requests), t0() + secs(5)).await;
        t.round(Some(&requests), t0() + secs(1900)).await; // given up
        assert_eq!(t.state.lock().await.link_requests.len(), 1);
        t.round(Some(&[]), t0() + secs(24 * 3600 + 10)).await;
        assert!(t.state.lock().await.link_requests.is_empty());
        let saved = State::load(&t.path()).await.unwrap();
        assert!(saved.link_requests.is_empty());
    }
}
