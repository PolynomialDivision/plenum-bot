//! 📋 plenum-bot: plenum protocols in Outline.
//!
//! Before every meeting of a plenum it creates the protocol from the plenum's template in
//! Outline and posts the link in the plenum's room; `!top` from the chat (also as a reply to
//! a message) lands on the agenda. After the meeting it copies the decisions into the
//! plenum's decision list, attaches a PDF and posts a summary; open tasks come along to the
//! next protocol. Several plenums (the house, the kitchens, the finance group), each with its
//! own room, area in the wiki and Nextcloud calendar. The calendar is where the dates are
//! (made, moved, cancelled, repeated there); the bot reads it over CalDAV and follows it. A
//! plenum with a directory group keeps its room's members and its wiki editors in line with
//! the group (the person's chosen account is invited), and answers `!link` in a direct chat
//! from an account on another server (the code that links it to a house user); an account a
//! person names in the identity provider's flow gets an invitation and that code without
//! `!link` (`link_delivery`).

mod calendar;
mod commands;
mod config;
mod engine;
mod link_delivery;
mod linking;
mod outline;
mod pdf;
mod protocol;
mod schedule;
mod state;
mod stay;

use std::{
    sync::Arc,
    time::{Duration, SystemTime},
};

use mxbot_common::directory::{self, Directory, Snapshot};

use anyhow::{Context, Result};
use mxbot_common::{
    admin::{Admins, Dispatch},
    matrix_sdk::{
        attachment::AttachmentConfig,
        deserialized_responses::EncryptionInfo,
        room::RoomMember,
        ruma::{
            events::{
                room::message::{
                    MessageType, OriginalSyncRoomMessageEvent, Relation, RoomMessageEventContent,
                },
                AnySyncMessageLikeEvent, AnySyncTimelineEvent, Mentions, SyncMessageLikeEvent,
            },
            OwnedRoomAliasId, OwnedRoomId, OwnedTransactionId, OwnedUserId, RoomAliasId, RoomId,
            UserId,
        },
        Client, Room, RoomMemberships, RoomState,
    },
    membership::Member,
    rooms::admin_dm_partner,
    supervise::{supervise_ticking, Alive},
    Bot,
};
use tracing::{error, info, warn};

use crate::{
    engine::{Chat, Engine, Scope, Sender},
    link_delivery::LinkChat,
    linking::LinkDesk,
    outline::Outline,
    state::State,
};

/// A message from Markdown (raw HTML in it, from a protocol, is shown as text); nobody is
/// pinged but `users`.
fn message(markdown: &str, users: &[OwnedUserId]) -> RoomMessageEventContent {
    mxbot_common::format::markdown(markdown)
        .add_mentions(Mentions::with_user_ids(users.iter().cloned()))
}

/// Matrix behind [`Chat`].
struct MatrixChat {
    client: Client,
    /// The bot's admins (configured and from `admin_groups`): whose direct chats get the admin
    /// notices.
    admins: Admins,
}

impl MatrixChat {
    fn room(&self, room: &RoomId) -> Result<Room> {
        self.client
            .get_room(room)
            .filter(|r| r.state() == RoomState::Joined)
            .with_context(|| format!("not in room {room} (make matrix-rooms joins it)"))
    }

    /// A direct chat of the bot with `user` that they are in (joined or invited), nobody else.
    fn direct_room(&self, user: &UserId) -> Option<Room> {
        self.client.joined_rooms().into_iter().find(|r| {
            r.active_members_count() == 2
                && r.direct_targets()
                    .iter()
                    .any(|t| t.as_user_id() == Some(user))
        })
    }

    /// Send with a transaction ID made from `key` and the room (stable across restarts).
    async fn send_keyed(&self, room: &Room, markdown: &str, key: &str) -> Result<()> {
        let txn = mxbot_common::send::txn_for(&format!("{key}:{}", room.room_id()));
        mxbot_common::send::send_with_retry_txn(room, message(markdown, &[]), &txn).await?;
        Ok(())
    }
}

fn is_admin(m: &RoomMember) -> bool {
    use mxbot_common::matrix_sdk::ruma::events::room::power_levels::UserPowerLevel;
    match m.power_level() {
        UserPowerLevel::Infinite => true,
        UserPowerLevel::Int(level) => i64::from(level) >= 100,
        _ => false,
    }
}

impl Chat for MatrixChat {
    async fn send(&self, room: &RoomId, markdown: &str, txn: &str) -> Result<()> {
        self.send_mentioning(room, markdown, &[], txn).await
    }

    async fn send_mentioning(
        &self,
        room: &RoomId,
        markdown: &str,
        users: &[OwnedUserId],
        txn: &str,
    ) -> Result<()> {
        let room = self.room(room)?;
        let content = message(markdown, users);
        let txn: OwnedTransactionId = txn.into();
        mxbot_common::send::send_with_retry_txn(&room, content, &txn).await?;
        Ok(())
    }

    async fn send_file(&self, room: &RoomId, name: &str, bytes: Vec<u8>, txn: &str) -> Result<()> {
        let room = self.room(room)?;
        let txn: OwnedTransactionId = txn.into();
        room.send_attachment(
            name,
            &mime::APPLICATION_PDF,
            bytes,
            AttachmentConfig::new()
                .txn_id(txn)
                .mentions(Some(Mentions::new())),
        )
        .await?;
        Ok(())
    }

    async fn members(&self, room: &RoomId) -> Result<Vec<Member>> {
        let room = self.room(room)?;
        let members = room
            .members(RoomMemberships::JOIN | RoomMemberships::INVITE)
            .await?;
        Ok(members
            .iter()
            .map(|m| Member {
                id: m.user_id().to_owned(),
                admin: is_admin(m),
                joined: *m.membership()
                    == mxbot_common::matrix_sdk::ruma::events::room::member::MembershipState::Join,
            })
            .collect())
    }

    async fn invite(&self, room: &RoomId, user: &UserId) -> Result<()> {
        self.room(room)?.invite_user_by_id(user).await?;
        Ok(())
    }

    async fn remove(&self, room: &RoomId, user: &UserId, reason: &str) -> Result<()> {
        self.room(room)?.kick_user(user, Some(reason)).await?;
        Ok(())
    }

    async fn tell_admins(&self, markdown: &str, key: &str) -> Result<()> {
        let rooms: Vec<Room> = self
            .client
            .joined_rooms()
            .into_iter()
            .filter(|r| admin_dm_partner(r, &self.admins.current()).is_some())
            .collect();
        if rooms.is_empty() {
            anyhow::bail!("no admin has a direct chat with me");
        }
        let mut sent = false;
        for room in &rooms {
            match self.send_keyed(room, markdown, key).await {
                Ok(()) => sent = true,
                Err(e) => warn!(room = %room.room_id(), "A notice to an admin failed: {e:#}"),
            }
        }
        if !sent {
            anyhow::bail!("no admin's direct chat took the notice");
        }
        Ok(())
    }

    async fn message_person(&self, user: &UserId, markdown: &str, key: &str) -> Result<()> {
        let room = match self.direct_room(user) {
            Some(room) => room,
            None => {
                mxbot_common::private::create_private_dm(
                    &self.client,
                    user,
                    &mxbot_common::private::PREFERRED_ROOM_VERSIONS,
                )
                .await?
            }
        };
        self.send_keyed(&room, markdown, key).await
    }

    async fn resolve_alias(&self, alias: &str) -> Result<OwnedRoomId> {
        let alias = RoomAliasId::parse(alias).with_context(|| format!("{alias:?} is no alias"))?;
        Ok(self.client.resolve_room_alias(&alias).await?.room_id)
    }
}

impl LinkChat for MatrixChat {
    async fn open(&self, account: &UserId) -> Result<OwnedRoomId> {
        // an existing chat only when it is private now (the code goes into it)
        let existing = match self.direct_room(account) {
            Some(room)
                if mxbot_common::private::is_private_dm(&self.client, room.room_id(), account)
                    .await =>
            {
                Some(room)
            }
            _ => None,
        };
        let room = match existing {
            Some(room) => room,
            None => {
                mxbot_common::private::create_private_dm(
                    &self.client,
                    account,
                    &mxbot_common::private::PREFERRED_ROOM_VERSIONS,
                )
                .await?
            }
        };
        Ok(room.room_id().to_owned())
    }

    async fn joined(&self, room: &RoomId, account: &UserId) -> Result<bool> {
        Ok(mxbot_common::private::is_private_dm(&self.client, room, account).await)
    }

    async fn send(&self, room: &RoomId, markdown: &str, key: &str) -> Result<()> {
        let room = self.room(room)?;
        self.send_keyed(&room, markdown, key).await
    }
}

const ADMIN_HELP: &str =
    "For everyone in the plenum rooms: !plenum · !top <topic> (also as a reply to a message) · \
!minutes done · !minutes confidential|link|full [day] · !plenum help. The meetings are in the plenums' Nextcloud calendars. \
Here in the admin chat !plenum shows every plenum, !members the rooms' held removals, the \
running handovers (a member's old account → the one they chose, until when it stays) and the \
stays that end soon or are overdue (the directory's stay_until), and \
!members confirm <plenum key|room> <digest> carries a room's held plan out once (the digest is \
in the notice and in !members). \
From an account on another server, !link <username> in a direct chat gives its link code; an \
account named in the settings (Matrix accounts) is invited and gets it there.";

type PlenumEngine = Engine<Outline, MatrixChat>;

/// Commands older than this (by the server's clock) are history, not orders: long enough for
/// those sent while the bot restarted, short of what a fresh store brings back.
pub(crate) const COMMAND_MAX_AGE: Duration = Duration::from_secs(15 * 60);
const COMMAND_MAX_AGE_MS: i64 = COMMAND_MAX_AGE.as_millis() as i64;

#[tokio::main]
async fn main() -> Result<()> {
    mxbot_common::logging::init("plenum_bot");
    let config: config::Config =
        mxbot_common::config::load_toml(&mxbot_common::config::config_path_from_args())?;
    let plenums = config.plenums()?;
    let tz = config.timezone()?;
    let outline = Outline::new(
        &config.outline.url,
        &config.public_url(),
        &config.api_key()?,
        Duration::from_secs(config.outline.timeout_secs),
    )?
    // nothing in the wiki open to everyone: the house group gets what everyone had
    // (subtenants are wiki members too, house-infra stacks/outline README "Subtenants")
    .with_house_group(config::wiki_house_group(&plenums)?);
    if config.pdf.enabled {
        for font in ["DejaVuSans.ttf", "DejaVuSans-Bold.ttf"] {
            let path = std::path::Path::new(&config.pdf.font_dir).join(font);
            if !path.exists() {
                warn!(
                    "PDF font {} is missing: protocols will be closed without a PDF",
                    path.display()
                );
            }
        }
    }
    let keep = config
        .members
        .keep
        .iter()
        .map(|u| {
            u.as_str()
                .try_into()
                .with_context(|| format!("[members] keep: {u:?} is no Matrix ID"))
        })
        .collect::<Result<_>>()?;
    config.members.check()?;
    let nextcloud = calendar::connect(
        &config.calendar.url,
        &config.calendar.username,
        &config.caldav_password()?,
    )?;

    let store_path = mxbot_common::config::store_path_from_env();
    let bot = Bot::builder("plenum-bot", env!("CARGO_PKG_VERSION"))
        .store_path(&store_path)
        .admin_help(ADMIN_HELP)
        .start(&config.matrix, &config.security)
        .await?;

    let state_path = store_path.join("state.json");
    let state = State::load(&state_path).await?;
    let mut engine = Engine::new(
        plenums,
        tz,
        outline,
        MatrixChat {
            client: bot.client.clone(),
            admins: bot.admin.admin_list().clone(),
        },
        config.pdf.clone(),
        bot.admin.admin_list().clone(),
        state,
        state_path,
    );
    if !config.calendar.web_url.trim().is_empty() {
        engine.calendar_url = Some(config.calendar.web_url.trim().to_owned());
    }
    if config.members.enabled {
        engine.me = Some(bot.user_id.clone());
        engine.keep = keep;
        engine.ask_to_choose = config.members.ask_to_choose;
        engine.choose_url = config.members.choose_url().map(str::to_owned);
        engine.grace = config.members.grace();
        engine.subtenant_groups = config.members.subtenant_groups();
        engine.stay_reminder_days = config.members.stay_reminder_days;
        if let Some(dir) = &config.directory {
            engine.stale_after = dir.stale_after();
        }
    }
    // `!link`: the codes for linking an account on another server to a house user
    let house_server = config
        .directory
        .as_ref()
        .and_then(|d| d.server_name.clone())
        .unwrap_or_else(|| bot.user_id.server_name().to_string());
    let link_desk = Arc::new(LinkDesk::from_env(
        &house_server,
        config.members.choose_url(),
    ));
    if !link_desk.enabled() {
        info!(
            "!link: no {} set, linking answers that it is not set up",
            linking::KEY_ENV
        );
    }
    // Link requests from the provider's flow: the bot invites the account and sends the code.
    // Direct chats from accounts the directory knows wait for its first read, so one that
    // came while the bot was down is not rejected for want of it.
    let link_requests = match (&config.directory, link_desk.key()) {
        (Some(dir), Some(key)) => match Directory::new(dir, &bot.user_id) {
            Ok(directory) => {
                bot.dm_accounts().wait_for_first_read();
                Some((
                    directory,
                    link_delivery::Desk {
                        key: key.to_vec(),
                        house_server: house_server.clone(),
                        choose_url: config.members.choose_url().map(str::to_owned),
                    },
                ))
            }
            Err(e) => {
                warn!("Link requests: {e:#}; none are served");
                None
            }
        },
        _ => {
            info!(
                "Link requests: no [directory] or no {}, none are served",
                linking::KEY_ENV
            );
            None
        }
    };
    let engine: Arc<PlenumEngine> = Arc::new(engine);
    for p in &engine.plenums {
        info!(plenum = %p.key, name = %p.name, room = %p.room, collection = %p.collection,
              calendar = %p.calendar, group = ?p.group, access = ?p.access, "Plenum");
    }

    let own_room = Arc::new(OwnRoom::of(&bot.user_id));
    install_handler(&bot, &engine, &link_desk, &own_room);
    info!("Starting sync...");
    bot.initial_sync().await;
    match own_room.id(&bot.client).await {
        Some(room) => info!(alias = %own_room.alias, %room, "The bot's own room"),
        None => {
            warn!(alias = %own_room.alias, "The bot's own room is not known yet (asked again when needed): !plenum there is not answered until it is")
        }
    }

    match engine.wiki.check().await {
        Ok(user) => info!(%user, "Outline answers"),
        Err(e) => warn!("Outline does not answer yet (tried again every minute): {e:#}"),
    }
    for p in &engine.plenums {
        if bot
            .client
            .get_room(&p.room)
            .is_none_or(|r| r.state() != RoomState::Joined)
        {
            warn!(plenum = %p.key, room = %p.room, "Not in the plenum's room: posts there fail until it is joined");
        }
    }
    // A loop that ends ends the bot (restarted by Docker); one without a round for three
    // pauses plus a slow round makes the container unhealthy.
    supervise_ticking(
        "plenum calendars",
        3 * (config.calendar.refresh() + SLOW_ROUND),
        |alive| {
            calendar_loop(
                Arc::clone(&engine),
                nextcloud,
                config.calendar.clone(),
                alive,
            )
        },
    );
    // A tick may wait on Outline and Matrix many times (a close with its PDF).
    supervise_ticking("plenum ticker", Duration::from_secs(15 * 60), |alive| {
        ticker(Arc::clone(&engine), alive)
    });
    match (&config.directory, config.members.enabled) {
        (Some(dir), true) if engine.plenums.iter().any(|p| p.group.is_some()) => {
            match Directory::new(dir, &bot.user_id) {
                Ok(directory) => {
                    let admins = (
                        bot.admin.admin_list().clone(),
                        config.security.admin_groups.clone(),
                    );
                    supervise_ticking(
                        "room members",
                        3 * (directory.config().refresh() + SLOW_ROUND),
                        |alive| members_loop(Arc::clone(&engine), directory, admins, alive),
                    );
                }
                Err(e) => warn!("Rooms: {e:#}; no room is managed"),
            }
        }
        _ => info!("Rooms: no plenum with a group, or [members] off: no room is managed"),
    }
    if let Some((directory, desk)) = link_requests {
        let chat = MatrixChat {
            client: bot.client.clone(),
            admins: bot.admin.admin_list().clone(),
        };
        let (bot, engine) = (bot.clone(), Arc::clone(&engine));
        let state_path = store_path.join("state.json");
        supervise_ticking(
            "link requests",
            3 * (link_delivery::ROUND + SLOW_ROUND),
            |alive| link_loop(bot, engine, chat, directory, desk, state_path, alive),
        );
    }
    bot.run().await
}

/// Every [`link_delivery::ROUND`]: the link requests (and every `refresh_minutes` the whole
/// directory, for the accounts that may open a direct chat and for who claims an account),
/// then a [`link_delivery::round`]. Only the people of the groups the bot serves
/// (`Engine::managed_groups`, the subtenants' too) may ask for a code or write to the bot. The
/// first good read takes up the direct-chat invites held for it. A directory that is down is another service's
/// outage (`Alive::unavailable`; the deliveries under way go on); one that refuses the token or
/// answers what the bot cannot read, and Matrix or the state failing, are this loop's failure.
async fn link_loop(
    bot: Bot,
    engine: Arc<PlenumEngine>,
    chat: MatrixChat,
    directory: Directory,
    desk: link_delivery::Desk,
    state_path: std::path::PathBuf,
    alive: Alive,
) {
    alive.expect_every(link_delivery::ROUND);
    let pending = mxbot_common::status::queue("link_codes");
    let skipped = mxbot_common::status::gauge("link_codes_skipped");
    let gave_up = mxbot_common::status::gauge("link_codes_gave_up");
    let groups = engine.managed_groups();
    if groups.is_empty() {
        warn!("Link requests: no plenum has a group, so nobody may ask for a code or write to me");
    }
    let mut snapshot: Option<Snapshot> = None;
    let mut next_read = std::time::Instant::now();
    let mut first = true;
    loop {
        alive.tick();
        // the directory down (another service's outage), or refusing or unreadable (ours)
        let mut unavailable: Vec<String> = Vec::new();
        let mut failed: Vec<String> = Vec::new();
        let mut sort = |what: &str, e: anyhow::Error| {
            let line = format!("{what}: {e:#}");
            if directory::is_outage(&e) {
                unavailable.push(line);
            } else {
                failed.push(line);
            }
        };
        if std::time::Instant::now() >= next_read {
            match directory.snapshot(&Default::default()).await {
                Ok(read) => {
                    bot.dm_accounts().follow(&read, directory.config(), &groups);
                    snapshot = Some(read);
                    next_read = std::time::Instant::now() + directory.config().refresh();
                    if std::mem::take(&mut first) {
                        let bot = bot.clone();
                        tokio::spawn(async move { bot.process_pending_invites().await });
                    }
                }
                Err(e) => {
                    sort("the directory", e);
                    next_read = std::time::Instant::now() + Duration::from_secs(60);
                }
            }
        }
        let now = SystemTime::now();
        // a read too old says nothing about who claims an account
        let people = snapshot
            .as_ref()
            .filter(|s| !s.is_stale(directory.config(), now))
            .map(|s| &s.people);
        let requests = match directory.link_requests(&groups).await {
            Ok(requests) => {
                bot.dm_accounts()
                    .announce(&requests, directory.server_name(), now);
                Some(requests)
            }
            Err(e) => {
                sort("link requests", e);
                None
            }
        };
        let report = link_delivery::round(
            &chat,
            &desk,
            &engine.state,
            &state_path,
            requests.as_deref(),
            people,
            chrono::Utc::now(),
        )
        .await;
        pending.set_since(report.pending, report.oldest.map(SystemTime::from));
        skipped.set(report.skipped_day as f64);
        gave_up.set(report.gave_up_day as f64);
        failed.extend(report.errors.iter().map(|e| format!("link requests: {e}")));
        if !failed.is_empty() {
            alive.failed(failed.join("; "));
        } else if !unavailable.is_empty() {
            alive.unavailable(unavailable.join("; "));
        } else if report.did_something() || report.pending > 0 {
            alive.ok();
        } else {
            alive.idle();
        }
        tokio::time::sleep(link_delivery::ROUND).await;
    }
}

/// The longest a round of the calendar or members loop normally takes, for their health check.
const SLOW_ROUND: Duration = Duration::from_secs(5 * 60);

/// Every minute, on the minute.
async fn ticker(engine: Arc<PlenumEngine>, alive: Alive) {
    alive.expect_every(Duration::from_secs(60));
    loop {
        alive.tick();
        engine.tick(chrono::Utc::now()).await;
        let secs = 60 - chrono::Utc::now().timestamp().rem_euclid(60) as u64;
        tokio::time::sleep(Duration::from_secs(secs.max(1))).await;
    }
}

/// Every plenum's calendar: made when missing and its shares kept in line with the plenum's
/// access (now and every hour, unless `[calendar] manage = false`), its occurrences from 30
/// days ago to 400 days ahead read every `refresh_minutes`. A calendar that cannot be read
/// changes nothing (the last answer stays).
async fn calendar_loop(
    engine: Arc<PlenumEngine>,
    nextcloud: calendar::Nextcloud,
    cfg: config::CalendarConfig,
    alive: Alive,
) {
    let every = cfg.refresh();
    alive.expect_every(every);
    let mut managed_at: Option<std::time::Instant> = None;
    loop {
        alive.tick();
        let mut unread: Vec<&str> = Vec::new();
        let manage =
            cfg.manage && managed_at.is_none_or(|at| at.elapsed() >= Duration::from_secs(3600));
        for p in &engine.plenums {
            if manage {
                match keep_calendar(&nextcloud, &cfg, p).await {
                    // A calendar made just now is empty: read at the next round, so that one
                    // someone deleted by mistake cancels nothing before it is noticed.
                    Ok(true) => {
                        warn!(plenum = %p.key, calendar = %p.calendar, "Calendar made (it was missing): its dates count once it has events");
                        engine.calendar_remade(&p.key).await;
                        continue;
                    }
                    Ok(false) => {}
                    Err(e) => {
                        warn!(plenum = %p.key, calendar = %p.calendar, "Calendar: {e:#}");
                        unread.push(&p.key);
                    }
                }
            }
            let today = chrono::Utc::now().with_timezone(&engine.tz).date_naive();
            let (from, to) = (
                today - chrono::Duration::days(30),
                today + chrono::Duration::days(400),
            );
            match nextcloud.events(&p.calendar, from, to, &engine.tz).await {
                Ok(events) => {
                    let occurrences = calendar::occurrences(&events, from, to);
                    engine
                        .apply_calendar(&p.key, occurrences, chrono::Utc::now())
                        .await;
                }
                Err(e) => {
                    warn!(plenum = %p.key, "Calendar not read ({e:#}); the dates stay as they were");
                    unread.push(&p.key);
                }
            }
        }
        if unread.is_empty() {
            alive.ok();
        } else {
            unread.dedup();
            alive.failed(format!("calendar not read: {}", unread.join(", ")));
        }
        if manage {
            managed_at = Some(std::time::Instant::now());
        }
        tokio::time::sleep(every).await;
    }
}

/// The calendar made when missing (true then) and its shares kept in line.
async fn keep_calendar(
    nextcloud: &calendar::Nextcloud,
    cfg: &config::CalendarConfig,
    p: &config::Plenum,
) -> Result<bool> {
    let made = nextcloud.ensure_calendar(&p.calendar, &p.name).await?;
    let wanted =
        calendar::wanted_shares(p.access, p.group.as_deref(), &cfg.house_group, &cfg.readers);
    let current = nextcloud.shares(&p.calendar).await?;
    let (set, remove) = calendar::plan_shares(&current, &wanted);
    for share in &set {
        nextcloud.share(&p.calendar, share).await?;
        info!(plenum = %p.key, principal = %share.principal, write = share.write, "Calendar shared");
    }
    for principal in &remove {
        nextcloud.unshare(&p.calendar, principal).await?;
        info!(plenum = %p.key, %principal, "Calendar share removed");
    }
    Ok(made)
}

/// The directory now and every `refresh_minutes` (one snapshot: its people and which groups
/// exist), the rooms after it. While it cannot be read, or a guard stops the round (a stale
/// read, a house group with nobody in it), the rooms stay as they are. A directory that is down
/// (unreachable, timing out, 5xx, 429: `directory::is_outage`) is another service's outage
/// (`Alive::unavailable`: in the status file and its alerts, never unhealthy, no restart); one
/// that refuses the token or answers what the bot cannot read, and a guard that stops the
/// round, count as failing (unhealthy after their time: someone has to fix the config or the
/// bot).
async fn members_loop(
    engine: Arc<PlenumEngine>,
    directory: Directory,
    (admins, admin_groups): (Admins, Vec<String>),
    alive: Alive,
) {
    alive.expect_every(directory.config().refresh());
    // the plenums' groups and the ones they require, and the subtenants' (a kitchen's room
    // requires them too: a group the read does not know leaves that room alone); the same the
    // link loop serves
    let wanted = engine.managed_groups();
    loop {
        let read = directory.snapshot(&wanted).await;
        // `[security] admin_groups`: their members' house accounts are admins while this read
        // may be used (a linked account never is).
        if let (Ok(snapshot), false) = (&read, admin_groups.is_empty()) {
            admins.follow(snapshot, directory.config(), &admin_groups);
        }
        let outage = read.as_ref().err().is_some_and(directory::is_outage);
        match engine.sync_members_after(read, chrono::Utc::now()).await {
            Ok(()) => alive.ok(),
            Err(why) if outage => alive.unavailable(format!("room members: {why}")),
            Err(why) => alive.failed(format!("room members: {why}")),
        }
        tokio::time::sleep(directory.config().refresh()).await;
    }
}

/// `!top` (maybe with a plenum key) as a reply: the message it answers is the topic, by its
/// author. None when it is no such reply or the message cannot be read.
async fn replied_topic(
    room: &Room,
    ev: &OriginalSyncRoomMessageEvent,
    body: &str,
    keys: &[&str],
) -> Option<(String, String)> {
    let mut words = body.split_whitespace();
    if words.next().map(str::to_lowercase).as_deref() != Some("!top") {
        return None;
    }
    let rest: Vec<&str> = words.collect();
    let key = match rest.as_slice() {
        [] => None,
        [k] if keys.contains(k) => Some(*k),
        _ => return None,
    };
    let Some(Relation::Reply(reply)) = &ev.content.relates_to else {
        return None;
    };
    let event = match room.event(&reply.in_reply_to.event_id, None).await {
        Ok(e) => e,
        Err(e) => {
            warn!("The message a !top answers could not be read: {e}");
            return None;
        }
    };
    let Ok(AnySyncTimelineEvent::MessageLike(AnySyncMessageLikeEvent::RoomMessage(
        SyncMessageLikeEvent::Original(original),
    ))) = event.raw().deserialize()
    else {
        return None;
    };
    let MessageType::Text(text) = &original.content.msgtype else {
        return None;
    };
    let topic = mxbot_common::format::strip_reply_fallback(&text.body).trim();
    if topic.is_empty() {
        return None;
    }
    let author = mxbot_common::format::fetch_names(room, &[original.sender.as_str()])
        .await
        .remove(original.sender.as_str())
        .unwrap_or_else(|| original.sender.localpart().to_owned());
    let command = match key {
        Some(k) => format!("!top {k} {topic}"),
        None => format!("!top {topic}"),
    };
    Some((command, author))
}

fn install_handler(
    bot: &Bot,
    engine: &Arc<PlenumEngine>,
    link_desk: &Arc<LinkDesk>,
    own_room: &Arc<OwnRoom>,
) {
    let bot = bot.clone();
    let engine = Arc::clone(engine);
    let link_desk = Arc::clone(link_desk);
    let own_room = Arc::clone(own_room);
    let rooms = engine.rooms();
    bot.client.clone().add_event_handler(
        move |ev: OriginalSyncRoomMessageEvent, room: Room, encryption: Option<EncryptionInfo>| {
            let bot = bot.clone();
            let engine = Arc::clone(&engine);
            let link_desk = Arc::clone(&link_desk);
            let own_room = Arc::clone(&own_room);
            let rooms = rooms.clone();
            async move {
                if room.state() != RoomState::Joined || ev.sender == bot.user_id {
                    return;
                }
                let dispatch = bot.admin.handle(&room, &ev, encryption.as_ref()).await;
                if dispatch == Dispatch::Handled {
                    return;
                }
                let MessageType::Text(text) = &ev.content.msgtype else {
                    return;
                };
                // A fresh Matrix store syncs the rooms' history: old commands are no orders
                // (an old `!minutes done` would close the meeting that is open now).
                let now = chrono::Utc::now();
                if now.timestamp_millis() - i64::from(ev.origin_server_ts.0) > COMMAND_MAX_AGE_MS {
                    info!(event = %ev.event_id, "An old message from the history, left alone");
                    return;
                }
                // `!link` from an account on another server, in a direct chat (anywhere else it
                // only says where). The code is in the answer only, never in the log.
                let in_plenum_room = rooms.contains(room.room_id());
                let direct = !in_plenum_room && room.active_members_count() == 2;
                let sent = ev
                    .origin_server_ts
                    .to_system_time()
                    .unwrap_or_else(SystemTime::now);
                let body = mxbot_common::format::strip_reply_fallback(&text.body);
                if let Some(answer) =
                    link_desk.answer(&ev.sender, body, direct, sent, SystemTime::now())
                {
                    info!(user = %ev.sender, room = %room.room_id(), direct, "!link answered");
                    if let Err(e) =
                        mxbot_common::send::send_with_retry(&room, message(&answer, &[])).await
                    {
                        error!("Answering in {} failed: {e}", room.room_id());
                    }
                    return;
                }
                // Plenum rooms, the admin DM, and the bot's own room (the overview); in any other
                // room (a direct chat from an account the directory knows) only `!link` above.
                if !in_plenum_room {
                    let admin_dm = dispatch == Dispatch::AdminDm;
                    let own = if admin_dm {
                        None
                    } else {
                        own_room.id(&bot.client).await
                    };
                    if !takes_commands(room.room_id(), admin_dm, own.as_deref()) {
                        return;
                    }
                }
                let keys: Vec<&str> = engine
                    .plenums
                    .iter()
                    .filter(|p| !in_plenum_room || p.room == room.room_id())
                    .map(|p| p.key.as_str())
                    .collect();
                // `!top` as the answer to a message: that message is the topic, by its author.
                let replied = if in_plenum_room {
                    replied_topic(&room, &ev, body, &keys).await
                } else {
                    None
                };
                let (body, author) = match &replied {
                    Some((command, author)) => (command.as_str(), Some(author.clone())),
                    None => (body, None),
                };
                let Some(command) = commands::parse_for(body, &keys) else {
                    return;
                };
                let name = match author {
                    Some(a) => a,
                    None => mxbot_common::format::fetch_names(&room, &[ev.sender.as_str()])
                        .await
                        .remove(ev.sender.as_str())
                        .unwrap_or_else(|| ev.sender.localpart().to_owned()),
                };
                let sender = Sender {
                    id: ev.sender.clone(),
                    name,
                };
                let scope = if dispatch == Dispatch::AdminDm {
                    Scope::AdminDm
                } else {
                    Scope::Room
                };
                let answer = engine
                    .command_in(room.room_id(), &sender, command, now, scope)
                    .await;
                let content = message(&answer, &[]);
                if let Err(e) = mxbot_common::send::send_with_retry(&room, content).await {
                    error!("Answering in {} failed: {e}", room.room_id());
                }
            }
        },
    );
}

/// The bot's own room, known by its room ID: the alias `#<bot localpart>:<bot server>` (`make
/// matrix-rooms` creates the room so) resolved on the bot's own homeserver, which alone says
/// where an alias on its server points. Never by a room's `m.room.canonical_alias`: that is
/// state whoever made the room sets, and a server other than the house's does not check it, so
/// a direct chat a stranger made (an account a link request names, say) could call itself the
/// bot's room and get `!plenum`'s overview.
///
/// Resolved once and kept; while it cannot be (the homeserver busy, the room not made yet) it
/// is asked again at most once a minute, and no room is the own room meanwhile.
struct OwnRoom {
    alias: OwnedRoomAliasId,
    id: std::sync::Mutex<Option<OwnedRoomId>>,
    tried: std::sync::Mutex<Option<std::time::Instant>>,
}

impl OwnRoom {
    fn of(bot: &UserId) -> Self {
        let alias = format!("#{}:{}", bot.localpart(), bot.server_name());
        Self {
            alias: RoomAliasId::parse(alias).expect("a user ID's parts make an alias"),
            id: std::sync::Mutex::new(None),
            tried: std::sync::Mutex::new(None),
        }
    }

    /// The room's ID, asked of the homeserver the first time (and after a failure, once a
    /// minute at most).
    async fn id(&self, client: &Client) -> Option<OwnedRoomId> {
        if let Some(id) = self.id.lock().unwrap().clone() {
            return Some(id);
        }
        {
            let mut tried = self.tried.lock().unwrap();
            if tried.is_some_and(|at| at.elapsed() < Duration::from_secs(60)) {
                return None;
            }
            *tried = Some(std::time::Instant::now());
        }
        match client.resolve_room_alias(&self.alias).await {
            Ok(answer) => {
                *self.id.lock().unwrap() = Some(answer.room_id.clone());
                Some(answer.room_id)
            }
            Err(e) => {
                warn!(alias = %self.alias, "The bot's own room could not be resolved: {e}");
                None
            }
        }
    }
}

/// Whether a message in `room`, which is no plenum's room, may be a command (`!plenum`, …):
/// in an admin's direct chat, and in the bot's own room, `own` (its ID, [`OwnRoom`]; `None`
/// while unknown). Anywhere else, a direct chat of an account the directory knows or one a
/// link request names, only `!link` is answered (before this is asked).
fn takes_commands(room: &RoomId, admin_dm: bool, own: Option<&RoomId>) -> bool {
    admin_dm || own == Some(room)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_own_room_is_known_by_its_id_and_nothing_else() {
        let own = <&RoomId>::try_from("!own:house.org").unwrap();
        // a direct chat a stranger made, whatever alias it gives itself: not the own room
        let forged = <&RoomId>::try_from("!forged:evil.example").unwrap();
        assert!(takes_commands(own, false, Some(own)));
        assert!(!takes_commands(forged, false, Some(own)));
        // while the own room is unknown, no room is it
        assert!(!takes_commands(own, false, None));
        // an admin's direct chat always
        assert!(takes_commands(forged, true, None));
        // the alias the ID is asked for: the bot's localpart on the bot's server
        let me = <&UserId>::try_from("@plenum-bot:house.org").unwrap();
        assert_eq!(OwnRoom::of(me).alias.as_str(), "#plenum-bot:house.org");
    }

    #[test]
    fn a_direct_chat_from_a_known_account_answers_only_link() {
        let desk = LinkDesk::new(Some(b"test-key".to_vec()), "house.org", None);
        let stranger = <&UserId>::try_from("@anna:matrix.org").unwrap();
        let dm = <&RoomId>::try_from("!dm:matrix.org").unwrap();
        let own = <&RoomId>::try_from("!own:house.org").unwrap();
        let now = SystemTime::now();
        // `!link` is answered there (with the code)
        let answer = desk.answer(stranger, "!link anna", true, now, now).unwrap();
        assert!(answer.contains("anna"), "{answer}");
        // anything else is no `!link`, and the room takes no commands: `!plenum` goes unanswered
        for body in ["!plenum", "!members", "!top x", "!plenum help"] {
            assert!(
                desk.answer(stranger, body, true, now, now).is_none(),
                "{body}"
            );
            assert!(!takes_commands(dm, false, Some(own)), "{body}");
        }
    }
}
