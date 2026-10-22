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
//! the group.

mod calendar;
mod commands;
mod config;
mod engine;
mod members;
mod outline;
mod pdf;
mod protocol;
mod schedule;
mod state;

use std::{sync::Arc, time::Duration};

use mxbot_common::directory::{self, Directory};

use anyhow::{Context, Result};
use mxbot_common::{
    admin::Dispatch,
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
            OwnedTransactionId, RoomId, UserId,
        },
        Client, Room, RoomMemberships, RoomState,
    },
    Bot,
};
use tracing::{error, info, warn};

use crate::{
    engine::{Chat, Engine, Scope, Sender},
    members::Member,
    outline::Outline,
    state::State,
};

/// A message from Markdown (raw HTML in it, from a protocol, is shown as text); nobody is
/// pinged.
fn message(markdown: &str) -> RoomMessageEventContent {
    mxbot_common::format::markdown(markdown).add_mentions(Mentions::new())
}

/// Matrix behind [`Chat`].
struct MatrixChat {
    client: Client,
}

impl MatrixChat {
    fn room(&self, room: &RoomId) -> Result<Room> {
        self.client
            .get_room(room)
            .filter(|r| r.state() == RoomState::Joined)
            .with_context(|| format!("not in room {room} (make matrix-rooms joins it)"))
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
        let room = self.room(room)?;
        let content = message(markdown);
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
}

const ADMIN_HELP: &str =
    "For everyone in the plenum rooms: !plenum · !top <topic> (also as a reply to a message) · \
!minutes done · !plenum help. The meetings are in the plenums' Nextcloud calendars. \
Here in the admin chat !plenum shows every plenum.";

type PlenumEngine = Engine<Outline, MatrixChat>;

/// Commands older than this (by the server's clock) are history, not orders: long enough for
/// those sent while the bot restarted, short of what a fresh store brings back.
const COMMAND_MAX_AGE_MS: i64 = 15 * 60 * 1000;

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
    )?;
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
        },
        config.pdf.clone(),
        bot.admins().clone(),
        state,
        state_path,
    );
    if !config.calendar.web_url.trim().is_empty() {
        engine.calendar_url = Some(config.calendar.web_url.trim().to_owned());
    }
    if config.members.enabled {
        engine.me = Some(bot.user_id.clone());
        engine.keep = keep;
    }
    let engine: Arc<PlenumEngine> = Arc::new(engine);
    for p in &engine.plenums {
        info!(plenum = %p.key, name = %p.name, room = %p.room, collection = %p.collection,
              calendar = %p.calendar, group = ?p.group, access = ?p.access, "Plenum");
    }

    install_handler(&bot, &engine);
    info!("Starting sync...");
    bot.initial_sync().await;

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
    tokio::spawn(calendar_loop(
        Arc::clone(&engine),
        nextcloud,
        config.calendar.clone(),
    ));
    tokio::spawn(ticker(Arc::clone(&engine)));
    match (&config.directory, config.members.enabled) {
        (Some(dir), true) if engine.plenums.iter().any(|p| p.group.is_some()) => {
            match Directory::new(dir, &bot.user_id) {
                Ok(directory) => {
                    tokio::spawn(members_loop(Arc::clone(&engine), directory));
                }
                Err(e) => warn!("Rooms: {e:#}; no room is managed"),
            }
        }
        _ => info!("Rooms: no plenum with a group, or [members] off: no room is managed"),
    }
    bot.run().await
}

/// Every minute, on the minute.
async fn ticker(engine: Arc<PlenumEngine>) {
    loop {
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
) {
    let every = Duration::from_secs(cfg.refresh_minutes.max(1) * 60);
    let mut managed_at: Option<std::time::Instant> = None;
    loop {
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
                    Err(e) => warn!(plenum = %p.key, calendar = %p.calendar, "Calendar: {e:#}"),
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
                    warn!(plenum = %p.key, "Calendar not read ({e:#}); the dates stay as they were")
                }
            }
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

/// The groups from the directory now and every `refresh_minutes`, the rooms after them.
/// While the directory cannot be read (its people or which groups exist), the rooms stay as
/// they are; a group that exists with nobody in it empties its room.
async fn members_loop(engine: Arc<PlenumEngine>, directory: Directory) {
    let wanted = config::directory_groups(&engine.plenums);
    loop {
        let read = async {
            let people = directory.people().await?;
            let existing = directory.existing_groups(&wanted).await?;
            anyhow::Ok(directory::groups_with_empty(&people, &wanted, &existing))
        };
        engine
            .sync_members_after(read.await, chrono::Utc::now())
            .await;
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

fn install_handler(bot: &Bot, engine: &Arc<PlenumEngine>) {
    let bot = bot.clone();
    let engine = Arc::clone(engine);
    let rooms = engine.rooms();
    bot.client.clone().add_event_handler(
        move |ev: OriginalSyncRoomMessageEvent, room: Room, encryption: Option<EncryptionInfo>| {
            let bot = bot.clone();
            let engine = Arc::clone(&engine);
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
                // Plenum rooms, the admin DM, and the bot's own room (the overview).
                let in_plenum_room = rooms.contains(room.room_id());
                if !in_plenum_room && dispatch != Dispatch::AdminDm && !own_room(&room, &bot) {
                    return;
                }
                let keys: Vec<&str> = engine
                    .plenums
                    .iter()
                    .filter(|p| !in_plenum_room || p.room == room.room_id())
                    .map(|p| p.key.as_str())
                    .collect();
                let body = mxbot_common::format::strip_reply_fallback(&text.body);
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
                let content = message(&answer);
                if let Err(e) = mxbot_common::send::send_with_retry(&room, content).await {
                    error!("Answering in {} failed: {e}", room.room_id());
                }
            }
        },
    );
}

/// The bot's own room: its alias is the bot's localpart (`make matrix-rooms` creates it so).
fn own_room(room: &Room, bot: &Bot) -> bool {
    room.canonical_alias()
        .is_some_and(|a| a.alias() == bot.user_id.localpart())
}
