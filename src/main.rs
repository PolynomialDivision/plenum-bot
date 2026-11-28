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
mod former;
mod link_delivery;
mod linking;
mod minutes;
mod outline;
mod page;
mod pdf;
mod plenum_settings;
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
        room::{IncludeRelations, RelationsOptions, RoomMember},
        ruma::{
            api::error::ErrorKind,
            events::{
                reaction::ReactionEventContent,
                relation::{Annotation, RelationType},
                room::pinned_events::RoomPinnedEventsEventContent,
                room::{
                    message::{
                        MessageType, OriginalSyncRoomMessageEvent, Relation, ReplacementMetadata,
                        RoomMessageEventContent,
                    },
                    redaction::OriginalSyncRoomRedactionEvent,
                },
                AnySyncMessageLikeEvent, AnySyncTimelineEvent, Mentions, SyncMessageLikeEvent,
            },
            OwnedEventId, OwnedRoomAliasId, OwnedRoomId, OwnedTransactionId, OwnedUserId,
            RoomAliasId, RoomId, UserId,
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
    commands::Command,
    engine::{Chat, ChatMessage, Engine, Notice, Origin, Scope, Sender},
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

/// [`message`], but the chat template as plain text: it keeps its lines when it is shown, and a
/// line followed by a lone `-` stays a line (as Markdown it would be a heading); its body, which
/// a phone copies, is the same either way.
fn message_for(markdown: &str, users: &[OwnedUserId], notice: Notice) -> RoomMessageEventContent {
    if notice == Notice::Template {
        RoomMessageEventContent::text_plain(markdown)
            .add_mentions(Mentions::with_user_ids(users.iter().cloned()))
    } else {
        message(markdown, users)
    }
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

// The engine decides what is sent (`engine::sends`); the notice only says what a message is.
impl Chat for MatrixChat {
    async fn send(&self, room: &RoomId, markdown: &str, notice: Notice, txn: &str) -> Result<()> {
        self.send_mentioning(room, markdown, &[], notice, txn)
            .await
            .map(|_| ())
    }

    async fn send_mentioning(
        &self,
        room: &RoomId,
        markdown: &str,
        users: &[OwnedUserId],
        notice: Notice,
        txn: &str,
    ) -> Result<String> {
        let room = self.room(room)?;
        let content = message_for(markdown, users, notice);
        let txn: OwnedTransactionId = txn.into();
        let event = mxbot_common::send::send_with_retry_txn(&room, content, &txn).await?;
        Ok(event.to_string())
    }

    async fn post(
        &self,
        room: &RoomId,
        markdown: &str,
        notice: Notice,
        txn: &str,
    ) -> Result<String> {
        let room = self.room(room)?;
        let txn: OwnedTransactionId = txn.into();
        let content = message_for(markdown, &[], notice);
        let event = mxbot_common::send::send_with_retry_txn(&room, content, &txn).await?;
        Ok(event.to_string())
    }

    async fn edit(&self, room: &RoomId, event: &str, markdown: &str, txn: &str) -> Result<()> {
        let room = self.room(room)?;
        let event = OwnedEventId::try_from(event)?;
        // nobody is pinged by the edit
        let content = message(markdown, &[])
            .make_replacement(ReplacementMetadata::new(event, Some(Mentions::new())));
        let txn: OwnedTransactionId = txn.into();
        mxbot_common::send::send_with_retry_txn(&room, content, &txn).await?;
        Ok(())
    }

    async fn react(&self, room: &RoomId, event: &str, key: &str, txn: &str) -> Result<String> {
        let room = self.room(room)?;
        let event = OwnedEventId::try_from(event)?;
        let content = ReactionEventContent::new(Annotation::new(event, key.to_owned()));
        let txn: OwnedTransactionId = txn.into();
        let sent = mxbot_common::send::send_with_retry_txn(&room, content, &txn).await?;
        Ok(sent.to_string())
    }

    async fn unreact(&self, room: &RoomId, reaction: &str, txn: &str) -> Result<()> {
        let room = self.room(room)?;
        let reaction = OwnedEventId::try_from(reaction)?;
        room.redact(&reaction, None, Some(txn.into())).await?;
        Ok(())
    }

    async fn reply(
        &self,
        room: &RoomId,
        thread: &str,
        event: &str,
        markdown: &str,
        txn: &str,
    ) -> Result<String> {
        let room = self.room(room)?;
        let content = mxbot_common::send::in_thread(
            message(markdown, &[]),
            OwnedEventId::try_from(thread)?,
            OwnedEventId::try_from(event)?,
        );
        let txn: OwnedTransactionId = txn.into();
        let sent = mxbot_common::send::send_with_retry_txn(&room, content, &txn).await?;
        Ok(sent.to_string())
    }

    // As cleaning-bot's `pin_weekly_plan`: one state event from the list the server has now
    // (the SDK's pin_event/unpin_event each start from a cached list).
    async fn set_pins(&self, room: &RoomId, pin: Option<&str>, unpin: &[String]) -> Result<bool> {
        let room = self.room(room)?;
        let pinned = room.load_pinned_events().await?.unwrap_or_default();
        let pin = pin.map(OwnedEventId::try_from).transpose()?;
        let mut wanted: Vec<OwnedEventId> = pinned
            .iter()
            .filter(|e| !unpin.iter().any(|u| u == e.as_str()) && Some(*e) != pin.as_ref())
            .cloned()
            .collect();
        wanted.extend(pin);
        if wanted == pinned {
            return Ok(true);
        }
        match room
            .send_state_event(RoomPinnedEventsEventContent::new(wanted))
            .await
        {
            Ok(_) => Ok(true),
            Err(e) if matches!(e.client_api_error_kind(), Some(ErrorKind::Forbidden)) => Ok(false),
            Err(e) => Err(e.into()),
        }
    }

    async fn send_file(
        &self,
        room: &RoomId,
        name: &str,
        bytes: Vec<u8>,
        _notice: Notice,
        txn: &str,
    ) -> Result<()> {
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
!minutes done · !minutes confidential|link|full [day] · !minutes as a reply (minutes from the \
chat) · !minutes undo · !minutes template · !plenum help. The meetings are in the plenums' Nextcloud calendars. \
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
    config.minutes.check()?;
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
    engine.minutes = config.minutes.clone();
    // never removed from a room, and never taken as minutes by themselves
    engine.keep = keep;
    if !config.calendar.web_url.trim().is_empty() {
        engine.calendar_url = Some(config.calendar.web_url.trim().to_owned());
    }
    if config.members.enabled {
        engine.me = Some(bot.user_id.clone());
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
        let report = engine.tick(chrono::Utc::now()).await;
        // a finished tick is a good round (without one, BotTaskLate fires half an hour after
        // every start); what failed in it is reported as such, an outage of the wiki apart
        if !report.failed.is_empty() {
            alive.failed(report.failed.join("; "));
        } else if !report.unavailable.is_empty() {
            alive.unavailable(report.unavailable.join("; "));
        } else {
            alive.ok();
        }
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

/// What a text message in a room is for the bot.
#[derive(Debug, PartialEq)]
enum Incoming {
    /// A new message: its text without a reply's fallback.
    Text { body: String },
    /// An edit of `target`: its new text, from `m.new_content` (never the fallback body, which
    /// starts with `* `).
    Edit { target: String, body: String },
}

/// The text of a message event: a new one or an edit; None for anything but text.
fn incoming(ev: &OriginalSyncRoomMessageEvent) -> Option<Incoming> {
    if let Some(Relation::Replacement(r)) = &ev.content.relates_to {
        let MessageType::Text(text) = &r.new_content.msgtype else {
            return None;
        };
        return Some(Incoming::Edit {
            target: r.event_id.to_string(),
            body: text.body.clone(),
        });
    }
    let MessageType::Text(text) = &ev.content.msgtype else {
        return None;
    };
    Some(Incoming::Text {
        body: mxbot_common::format::strip_reply_fallback(&text.body).to_owned(),
    })
}

/// The message a reply answers: a plain reply's, or a thread message's that answers one
/// (not the thread's latest a client names as a fallback).
fn replied_to(ev: &OriginalSyncRoomMessageEvent) -> Option<OwnedEventId> {
    match &ev.content.relates_to {
        Some(Relation::Reply(reply)) => Some(reply.in_reply_to.event_id.clone()),
        Some(Relation::Thread(thread)) if !thread.is_falling_back => {
            thread.in_reply_to.as_ref().map(|r| r.event_id.clone())
        }
        _ => None,
    }
}

/// The message a redaction deletes (in the event or, in newer rooms, its content).
fn redacted(ev: &OriginalSyncRoomRedactionEvent) -> Option<String> {
    ev.redacts
        .as_ref()
        .or(ev.content.redacts.as_ref())
        .map(|id| id.to_string())
}

/// Whether a command is `!minutes …`.
fn is_minutes_command(body: &str) -> bool {
    body.split_whitespace()
        .next()
        .is_some_and(|w| w.eq_ignore_ascii_case("!minutes"))
}

/// The name a room shows for `user`.
async fn name_of(room: &Room, user: &UserId) -> String {
    mxbot_common::format::fetch_names(room, &[user.as_str()])
        .await
        .remove(user.as_str())
        .unwrap_or_else(|| user.localpart().to_owned())
}

/// The message a `!minutes` command is about: the one it answers. In a thread, `!minutes
/// undo` and `!minutes <day>` without an answer are about the thread's first message (the
/// bot's thread reply says to answer there); a bare `!minutes` in a thread stays the overview.
///
/// With chat minutes switched off (`from_chat` false) a bare `!minutes` is about no message:
/// as an answer too it is the overview it always was.
fn minutes_target(
    ev: &OriginalSyncRoomMessageEvent,
    command: &Command,
    from_chat: bool,
) -> Option<OwnedEventId> {
    if !from_chat && matches!(command, Command::Overview(_)) {
        return None;
    }
    if let Some(target) = replied_to(ev) {
        return Some(target);
    }
    let in_thread = matches!(&ev.content.relates_to, Some(Relation::Thread(_)));
    let about_minutes = matches!(command, Command::Undo(_) | Command::Take { text: None, .. });
    (in_thread && about_minutes).then(|| mxbot_common::send::thread_root(ev))
}

/// The text `original` shows now: its sender's latest edit among `edits`, else None.
fn latest_edit(
    original: &OriginalSyncRoomMessageEvent,
    edits: impl IntoIterator<Item = OriginalSyncRoomMessageEvent>,
) -> Option<String> {
    edits
        .into_iter()
        .filter(|e| e.sender == original.sender)
        .filter_map(|e| match incoming(&e) {
            Some(Incoming::Edit { target, body }) if target == original.event_id.as_str() => {
                Some((e.origin_server_ts, body))
            }
            _ => None,
        })
        .max_by_key(|(at, _)| *at)
        .map(|(_, body)| body)
}

/// A message of the room as the bot can read it.
async fn room_message(room: &Room, id: &OwnedEventId) -> Option<OriginalSyncRoomMessageEvent> {
    let event = match room.event(id, None).await {
        Ok(e) => e,
        Err(e) => {
            warn!("The message a !minutes is about could not be read: {e}");
            return None;
        }
    };
    match event.raw().deserialize() {
        Ok(AnySyncTimelineEvent::MessageLike(AnySyncMessageLikeEvent::RoomMessage(
            SyncMessageLikeEvent::Original(original),
        ))) => Some(original),
        _ => None,
    }
}

/// The edits of `original` the homeserver has (the newest ten; decrypted by the SDK). None
/// read: the message counts as it was sent.
async fn edits_of(
    room: &Room,
    original: &OriginalSyncRoomMessageEvent,
) -> Vec<OriginalSyncRoomMessageEvent> {
    let opts = RelationsOptions {
        include_relations: IncludeRelations::RelationsOfType(RelationType::Replacement),
        ..Default::default()
    };
    match room.relations(original.event_id.clone(), opts).await {
        Ok(relations) => (relations.chunk.iter())
            .filter_map(|e| match e.raw().deserialize() {
                Ok(AnySyncTimelineEvent::MessageLike(AnySyncMessageLikeEvent::RoomMessage(
                    SyncMessageLikeEvent::Original(edit),
                ))) => Some(edit),
                _ => None,
            })
            .collect(),
        Err(e) => {
            warn!("The edits of a message could not be read (taken as it was sent): {e}");
            Vec::new()
        }
    }
}

/// The message `target` as minutes for a `!minutes` about it, as it reads now (its latest
/// edit). None: nothing a person wrote (the bot's own message; its thread reply stands for
/// the message it answers). Err: what to say.
async fn replied_minutes(
    room: &Room,
    target: &OwnedEventId,
    me: &UserId,
    keys: &[&str],
) -> Result<Option<ChatMessage>, String> {
    const UNREADABLE: &str =
        "⚠️ I cannot read the message you answered (yet). Please try again in a moment.";
    let Some(mut original) = room_message(room, target).await else {
        return Err(UNREADABLE.to_owned());
    };
    if original.sender == me {
        // the bot's thread reply: the minutes are the thread's first message
        let root = mxbot_common::send::thread_root(&original);
        if root == original.event_id {
            return Ok(None);
        }
        match room_message(room, &root).await {
            Some(first) if first.sender != me => original = first,
            Some(_) => return Ok(None),
            None => return Err(UNREADABLE.to_owned()),
        }
    }
    let Some(Incoming::Text { body }) = incoming(&original) else {
        return Err("Only a text message can be minutes.".to_owned());
    };
    let body = latest_edit(&original, edits_of(room, &original).await).unwrap_or(body);
    Ok(Some(ChatMessage {
        event: original.event_id.to_string(),
        thread: mxbot_common::send::thread_root(&original).to_string(),
        sender: original.sender.clone(),
        sender_name: name_of(room, &original.sender).await,
        body: minutes::without_command(&body, keys).to_owned(),
        sent_at: original
            .origin_server_ts
            .to_system_time()
            .map_or_else(chrono::Utc::now, chrono::DateTime::<chrono::Utc>::from),
    }))
}

fn install_handler(
    bot: &Bot,
    engine: &Arc<PlenumEngine>,
    link_desk: &Arc<LinkDesk>,
    own_room: &Arc<OwnRoom>,
) {
    let client = bot.client.clone();
    let bot = bot.clone();
    let redaction_engine = Arc::clone(engine);
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
                let Some(incoming) = incoming(&ev) else {
                    return;
                };
                let now = chrono::Utc::now();
                let in_plenum_room = rooms.contains(room.room_id());
                let sent_at = ev
                    .origin_server_ts
                    .to_system_time()
                    .map_or(now, chrono::DateTime::<chrono::Utc>::from);
                // Minutes are no commands: taken whatever their age (the bot was away), but not
                // what the first sync of a fresh store brings back, nor a keep account's.
                let minutes_from = in_plenum_room
                    && engine.minutes.from_chat
                    && !mxbot_common::events::is_replayed(ev.origin_server_ts)
                    && !engine.keep.contains(&ev.sender);
                let body = match incoming {
                    Incoming::Edit { target, body } => {
                        if minutes_from {
                            let name = name_of(&room, &ev.sender).await;
                            // an edit that makes minutes of a message: answered in that
                            // message's own thread (unreadable: its own)
                            let thread = match OwnedEventId::try_from(target.as_str()) {
                                Ok(id) if minutes::is_minutes(&body) => room_message(&room, &id)
                                    .await
                                    .map(|o| mxbot_common::send::thread_root(&o).to_string()),
                                _ => None,
                            };
                            engine
                                .edited_in_thread(
                                    room.room_id(),
                                    &target,
                                    thread.as_deref(),
                                    &ev.sender,
                                    &name,
                                    &body,
                                    sent_at,
                                    now,
                                )
                                .await;
                        }
                        return;
                    }
                    Incoming::Text { body } => body,
                };
                if in_plenum_room && !body.trim_start().starts_with('!') {
                    if minutes_from && minutes::is_minutes(&body) {
                        let m = ChatMessage {
                            event: ev.event_id.to_string(),
                            thread: mxbot_common::send::thread_root(&ev).to_string(),
                            sender: ev.sender.clone(),
                            sender_name: name_of(&room, &ev.sender).await,
                            body,
                            sent_at,
                        };
                        engine.message(room.room_id(), m, now).await;
                    }
                    return;
                }
                let body = body.as_str();
                // A fresh Matrix store syncs the rooms' history: old commands are no orders
                // (an old `!minutes done` would close the meeting that is open now).
                if now.timestamp_millis() - i64::from(ev.origin_server_ts.0) > COMMAND_MAX_AGE_MS {
                    info!(event = %ev.event_id, "An old message from the history, left alone");
                    return;
                }
                // `!link` from an account on another server, in a direct chat (anywhere else it
                // only says where). The code is in the answer only, never in the log.
                let direct = !in_plenum_room && room.active_members_count() == 2;
                let sent = ev
                    .origin_server_ts
                    .to_system_time()
                    .unwrap_or_else(SystemTime::now);
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
                    None => name_of(&room, &ev.sender).await,
                };
                // `!minutes` about a message: the one it answers, or its own text
                let mut origin = Origin::default();
                if in_plenum_room && is_minutes_command(body) {
                    match &command {
                        Command::Take {
                            text: Some(text), ..
                        } => {
                            origin.this = Some(ChatMessage {
                                event: ev.event_id.to_string(),
                                thread: mxbot_common::send::thread_root(&ev).to_string(),
                                sender: ev.sender.clone(),
                                sender_name: name.clone(),
                                body: text.clone(),
                                sent_at,
                            })
                        }
                        Command::Overview(_) | Command::Take { .. } | Command::Undo(_) => {
                            let on = engine.minutes.from_chat;
                            if let Some(target) = minutes_target(&ev, &command, on) {
                                match replied_minutes(&room, &target, &bot.user_id, &keys).await {
                                    Ok(m) => origin.replied = m,
                                    Err(why) => {
                                        let content = message(&why, &[]);
                                        if let Err(e) =
                                            mxbot_common::send::send_with_retry(&room, content)
                                                .await
                                        {
                                            error!("Answering in {} failed: {e}", room.room_id());
                                        }
                                        return;
                                    }
                                }
                            }
                        }
                        _ => {}
                    }
                }
                let sender = Sender {
                    id: ev.sender.clone(),
                    name,
                };
                let scope = if dispatch == Dispatch::AdminDm {
                    Scope::AdminDm
                } else {
                    Scope::Room
                };
                for answer in engine
                    .answer(room.room_id(), &sender, command, origin, now, scope)
                    .await
                {
                    let content = message(&answer, &[]);
                    if let Err(e) = mxbot_common::send::send_with_retry(&room, content).await {
                        error!("Answering in {} failed: {e}", room.room_id());
                    }
                }
            }
        },
    ); // A deleted minutes message is withdrawn (docs/CHAT-MINUTES.md).
    let engine = redaction_engine;
    client.add_event_handler(move |ev: OriginalSyncRoomRedactionEvent, room: Room| {
        let engine = Arc::clone(&engine);
        async move {
            if !engine.minutes.from_chat || !engine.rooms().contains(room.room_id()) {
                return;
            }
            if let Some(target) = redacted(&ev) {
                engine
                    .redacted(room.room_id(), &target, chrono::Utc::now())
                    .await;
            }
        }
    });
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

    fn message_event(content: serde_json::Value) -> OriginalSyncRoomMessageEvent {
        serde_json::from_value(serde_json::json!({
            "type": "m.room.message",
            "event_id": "$new",
            "sender": "@anna:example.org",
            "origin_server_ts": 1_800_000_000_000u64,
            "content": content,
        }))
        .unwrap()
    }

    #[test]
    fn the_chat_template_is_sent_as_plain_text() {
        let template = "Minutes House plenum\nDecisions:\n-\n\nTasks:\n-";
        let json = serde_json::to_value(message_for(template, &[], Notice::Template)).unwrap();
        assert!(json.get("formatted_body").is_none(), "{json}");
        assert_eq!(json["body"], template);
        let json = serde_json::to_value(message_for("**x**", &[], Notice::Link)).unwrap();
        assert!(json.get("formatted_body").is_some(), "{json}");
    }

    #[test]
    fn the_three_event_kinds_are_read_from_their_json() {
        // an original message, also a reply (its fallback dropped)
        let ev = message_event(serde_json::json!({"msgtype": "m.text", "body": "Protokoll\nx"}));
        assert_eq!(
            incoming(&ev),
            Some(Incoming::Text {
                body: "Protokoll\nx".into()
            })
        );
        assert_eq!(mxbot_common::send::thread_root(&ev).as_str(), "$new");
        assert_eq!(replied_to(&ev), None);
        let ev = message_event(serde_json::json!({
            "msgtype": "m.text",
            "body": "> <@ben:example.org> Protokoll\n\n!minutes",
            "m.relates_to": {"m.in_reply_to": {"event_id": "$minutes"}},
        }));
        assert_eq!(
            incoming(&ev),
            Some(Incoming::Text {
                body: "!minutes".into()
            })
        );
        assert_eq!(replied_to(&ev).unwrap().as_str(), "$minutes");

        // a replacement: the new content, not the "* " fallback
        let ev = message_event(serde_json::json!({
            "msgtype": "m.text",
            "body": "* Protokoll neu",
            "m.new_content": {"msgtype": "m.text", "body": "Protokoll neu"},
            "m.relates_to": {"rel_type": "m.replace", "event_id": "$old"},
        }));
        assert_eq!(
            incoming(&ev),
            Some(Incoming::Edit {
                target: "$old".into(),
                body: "Protokoll neu".into()
            })
        );

        // a thread reply: answered in its thread; a reply to a thread message only when it
        // answers one, not the client's fallback
        let ev = message_event(serde_json::json!({
            "msgtype": "m.text",
            "body": "Protokoll im Thread",
            "m.relates_to": {"rel_type": "m.thread", "event_id": "$root",
                             "is_falling_back": true, "m.in_reply_to": {"event_id": "$last"}},
        }));
        assert_eq!(mxbot_common::send::thread_root(&ev).as_str(), "$root");
        assert_eq!(replied_to(&ev), None);
        let ev = message_event(serde_json::json!({
            "msgtype": "m.text",
            "body": "!minutes",
            "m.relates_to": {"rel_type": "m.thread", "event_id": "$root",
                             "is_falling_back": false, "m.in_reply_to": {"event_id": "$part2"}},
        }));
        assert_eq!(replied_to(&ev).unwrap().as_str(), "$part2");

        // no text: nothing
        let ev = message_event(
            serde_json::json!({"msgtype": "m.image", "body": "x.png", "url": "mxc://example.org/a"}),
        );
        assert_eq!(incoming(&ev), None);

        // a redaction, in the event (older rooms) or in its content (v11)
        for ev in [
            serde_json::json!({"type": "m.room.redaction", "event_id": "$r", "sender": "@anna:example.org",
                               "origin_server_ts": 1, "redacts": "$minutes", "content": {}}),
            serde_json::json!({"type": "m.room.redaction", "event_id": "$r", "sender": "@anna:example.org",
                               "origin_server_ts": 1, "content": {"redacts": "$minutes"}}),
        ] {
            let ev: OriginalSyncRoomRedactionEvent = serde_json::from_value(ev).unwrap();
            assert_eq!(redacted(&ev).as_deref(), Some("$minutes"));
        }
        assert!(is_minutes_command("!Minutes 15.11."));
        assert!(!is_minutes_command("!plenum"));
    }

    #[test]
    fn a_minutes_command_in_a_thread_is_about_its_first_message() {
        let in_thread = |body: &str| {
            message_event(serde_json::json!({
                "msgtype": "m.text",
                "body": body,
                "m.relates_to": {"rel_type": "m.thread", "event_id": "$minutes",
                                 "is_falling_back": true, "m.in_reply_to": {"event_id": "$receipt"}},
            }))
        };
        let target = |body: &str| {
            let command = commands::parse_for(body, &[]).unwrap();
            minutes_target(&in_thread(body), &command, true).map(|id| id.to_string())
        };
        assert_eq!(target("!minutes undo").as_deref(), Some("$minutes"));
        assert_eq!(target("!minutes 15.11.").as_deref(), Some("$minutes"));
        // a bare `!minutes` there is the overview, as anywhere
        assert_eq!(target("!minutes"), None);
        // an answer names its message, in a thread or not
        let ev = message_event(serde_json::json!({
            "msgtype": "m.text",
            "body": "!minutes undo",
            "m.relates_to": {"m.in_reply_to": {"event_id": "$other"}},
        }));
        let undo = commands::parse_for("!minutes undo", &[]).unwrap();
        assert_eq!(minutes_target(&ev, &undo, true).unwrap().as_str(), "$other");
        // outside a thread and no answer: none
        let ev = message_event(serde_json::json!({"msgtype": "m.text", "body": "!minutes undo"}));
        assert_eq!(minutes_target(&ev, &undo, true), None);
        // switched off: `!minutes` as an answer is the overview, whatever it answers
        let ev = message_event(serde_json::json!({
            "msgtype": "m.text",
            "body": "!minutes",
            "m.relates_to": {"m.in_reply_to": {"event_id": "$other"}},
        }));
        let bare = commands::parse_for("!minutes", &[]).unwrap();
        assert_eq!(minutes_target(&ev, &bare, false), None);
        assert_eq!(minutes_target(&ev, &bare, true).unwrap().as_str(), "$other");
    }

    #[test]
    fn a_message_reads_as_its_senders_latest_edit() {
        let original = message_event(serde_json::json!({"msgtype": "m.text", "body": "Notes"}));
        let edit =
            |sender: &str, at: u64, target: &str, body: &str| -> OriginalSyncRoomMessageEvent {
                serde_json::from_value(serde_json::json!({
                    "type": "m.room.message",
                    "event_id": format!("$edit{at}"),
                    "sender": sender,
                    "origin_server_ts": at,
                    "content": {
                        "msgtype": "m.text",
                        "body": format!("* {body}"),
                        "m.new_content": {"msgtype": "m.text", "body": body},
                        "m.relates_to": {"rel_type": "m.replace", "event_id": target},
                    },
                }))
                .unwrap()
            };
        let anna = "@anna:example.org";
        assert_eq!(latest_edit(&original, Vec::new()), None);
        let edits = vec![
            edit(anna, 1_800_000_002_000, "$new", "Protokoll, zweite Fassung"),
            edit(anna, 1_800_000_001_000, "$new", "Protokoll, erste Fassung"),
            // not the sender's, and not of this message
            edit(
                "@ben:example.org",
                1_800_000_009_000,
                "$new",
                "Bens Fassung",
            ),
            edit(anna, 1_800_000_009_000, "$else", "eine andere Nachricht"),
        ];
        assert_eq!(
            latest_edit(&original, edits).as_deref(),
            Some("Protokoll, zweite Fassung")
        );
    }

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
