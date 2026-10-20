//! 📋 plenum-bot: plenum protocols in Outline.
//!
//! Before every meeting of a plenum it creates the protocol from the plenum's template in
//! Outline and posts the link in the plenum's room; `!top` from the chat lands on the
//! agenda. After the meeting it copies the decisions into the plenum's decision list,
//! attaches a PDF and posts a summary. Searching happens in Outline. Several plenums (the
//! house, the kitchens), each with its own room, collection and rhythm.

mod commands;
mod config;
mod engine;
mod outline;
mod pdf;
mod protocol;
mod schedule;
mod state;

use std::{sync::Arc, time::Duration};

use anyhow::{Context, Result};
use mxbot_common::{
    admin::Dispatch,
    matrix_sdk::{
        attachment::AttachmentConfig,
        deserialized_responses::EncryptionInfo,
        ruma::{
            events::{
                room::message::{
                    MessageType, OriginalSyncRoomMessageEvent, RoomMessageEventContent,
                },
                Mentions,
            },
            OwnedTransactionId, RoomId,
        },
        Client, Room, RoomState,
    },
    Bot,
};
use tracing::{error, info, warn};

use crate::{
    engine::{Chat, Engine, Sender},
    outline::Outline,
    state::State,
};

/// A message from Markdown: the Markdown as the plain body, HTML for clients. Raw HTML in
/// it (from a protocol, typed by people) is shown as text, and nobody is pinged.
fn message(markdown: &str) -> RoomMessageEventContent {
    use pulldown_cmark::{Event, Options, Parser};
    let events = Parser::new_ext(markdown, Options::ENABLE_STRIKETHROUGH).map(|e| match e {
        Event::Html(t) | Event::InlineHtml(t) => Event::Text(t),
        other => other,
    });
    let mut html = String::new();
    pulldown_cmark::html::push_html(&mut html, events);
    RoomMessageEventContent::text_html(markdown, html.trim_end()).add_mentions(Mentions::new())
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
}

const ADMIN_HELP: &str =
    "Für alle in den Plenum-Räumen: !plenum · !top <Thema> · !protokoll fertig · \
!plenum absagen <Datum> · !plenum extra <Datum> <Uhrzeit> · !plenum hilfe. \
Hier im Admin-Chat zeigt !plenum alle Plena.";

type PlenumEngine = Engine<Outline, MatrixChat>;

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

    let store_path = mxbot_common::config::store_path_from_env();
    let bot = Bot::builder("plenum-bot", env!("CARGO_PKG_VERSION"))
        .store_path(&store_path)
        .admin_help(ADMIN_HELP)
        .start(&config.matrix, &config.security)
        .await?;

    let state_path = store_path.join("state.json");
    let state = State::load(&state_path).await?;
    let engine: Arc<PlenumEngine> = Arc::new(Engine::new(
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
    ));
    for p in &engine.plenums {
        info!(plenum = %p.key, name = %p.name, room = %p.room, collection = %p.collection, "Plenum");
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
    tokio::spawn(ticker(Arc::clone(&engine)));
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
                // Plenum rooms, the admin DM, and the bot's own room (the overview).
                let in_plenum_room = rooms.contains(room.room_id());
                if !in_plenum_room && dispatch != Dispatch::AdminDm && !own_room(&room, &bot) {
                    return;
                }
                let now = chrono::Utc::now();
                let today = now.with_timezone(&engine.tz).date_naive();
                let keys: Vec<&str> = engine
                    .plenums
                    .iter()
                    .filter(|p| !in_plenum_room || p.room == room.room_id())
                    .map(|p| p.key.as_str())
                    .collect();
                let Some(command) = commands::parse_for(&text.body, today, &keys) else {
                    return;
                };
                let name = mxbot_common::format::fetch_names(&room, &[ev.sender.as_str()])
                    .await
                    .remove(ev.sender.as_str())
                    .unwrap_or_else(|| ev.sender.localpart().to_owned());
                let sender = Sender {
                    id: ev.sender.clone(),
                    name,
                };
                let answer = engine.command(room.room_id(), &sender, command, now).await;
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
