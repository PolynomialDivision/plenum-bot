//! The Outline API (`POST <url>/api/<method>`, JSON, `Authorization: Bearer <key>`), only
//! the calls the bot needs. Checked against Outline 1.11.0 (2026-10-20): a document can be
//! created with an ID of our own, `documents.update` with `lastRevision` answers 409 when
//! someone changed the document meanwhile, a deleted document still answers `documents.info`
//! (with `deletedAt`), and an attachment is a `attachments.create` plus a multipart upload.

use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::{
    config::Access,
    engine::{Doc, Wiki},
};

pub struct Outline {
    http: reqwest::Client,
    base: String,
    key: String,
    public: String,
}

/// An answer that is not 200.
#[derive(Debug)]
pub struct ApiError {
    pub status: u16,
    pub error: String,
    pub message: String,
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Outline {} {}: {}",
            self.status, self.error, self.message
        )
    }
}

impl std::error::Error for ApiError {}

#[derive(Deserialize)]
struct Envelope<T> {
    data: T,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ApiDoc {
    id: String,
    title: String,
    #[serde(default)]
    text: String,
    url: String,
    #[serde(default)]
    collection_id: Option<String>,
    #[serde(default)]
    parent_document_id: Option<String>,
    #[serde(default)]
    deleted_at: Option<String>,
    #[serde(default)]
    archived_at: Option<String>,
    #[serde(default)]
    created_at: Option<String>,
    #[serde(default)]
    updated_at: Option<String>,
}

impl From<ApiDoc> for Doc {
    fn from(d: ApiDoc) -> Self {
        Doc {
            id: d.id,
            title: d.title,
            text: d.text,
            url: d.url,
            // Every change sets updatedAt, a new title too; the revision count is no help
            // (Outline 1.11 counts it up in the background after creating, seen 2026-10-22).
            untouched: d.created_at.is_some() && d.updated_at == d.created_at,
            collection_id: d.collection_id,
            parent_id: d.parent_document_id,
        }
    }
}

#[derive(Deserialize)]
struct ApiCollection {
    id: String,
    name: String,
    /// "read_write", "read", or null (only its members and groups).
    #[serde(default)]
    permission: Option<String>,
    #[serde(default, rename = "deletedAt")]
    deleted_at: Option<String>,
    #[serde(default, rename = "archivedAt")]
    archived_at: Option<String>,
}

#[derive(Deserialize)]
struct ApiGroup {
    id: String,
    name: String,
}

#[derive(Deserialize)]
struct GroupList {
    #[serde(default)]
    groups: Vec<ApiGroup>,
}

/// The collection permission for everyone in the workspace.
fn permission(access: Access) -> Option<&'static str> {
    match access {
        Access::House => Some("read_write"),
        Access::Members => Some("read"),
        Access::Private => None,
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Upload {
    #[serde(default)]
    mode: Option<String>,
    #[serde(default)]
    upload_url: Option<String>,
    #[serde(default)]
    form: serde_json::Map<String, Value>,
    attachment: Attachment,
}

#[derive(Deserialize)]
struct Attachment {
    url: String,
}

impl Outline {
    pub fn new(base: &str, public: &str, key: &str, timeout: Duration) -> Result<Self> {
        Ok(Self {
            http: reqwest::Client::builder()
                .user_agent(concat!("plenum-bot/", env!("CARGO_PKG_VERSION")))
                .timeout(timeout)
                .build()?,
            base: base.trim_end_matches('/').to_owned(),
            key: key.to_owned(),
            public: public.trim_end_matches('/').to_owned(),
        })
    }

    async fn call<T: for<'de> Deserialize<'de>>(&self, method: &str, body: Value) -> Result<T> {
        let resp = self
            .http
            .post(format!("{}/api/{method}", self.base))
            .bearer_auth(&self.key)
            .json(&body)
            .send()
            .await
            .with_context(|| format!("Outline {method}"))?;
        let status = resp.status();
        let bytes = resp
            .bytes()
            .await
            .with_context(|| format!("Outline {method}"))?;
        if !status.is_success() {
            let v: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
            return Err(ApiError {
                status: status.as_u16(),
                error: v["error"].as_str().unwrap_or("").to_owned(),
                message: v["message"].as_str().unwrap_or("").to_owned(),
            }
            .into());
        }
        let env: Envelope<T> = serde_json::from_slice(&bytes)
            .with_context(|| format!("Outline {method}: unexpected answer"))?;
        Ok(env.data)
    }

    /// Whether the API answers with the key at all (startup check).
    pub async fn check(&self) -> Result<String> {
        let v: Value = self.call("auth.info", json!({})).await?;
        Ok(v["user"]["name"].as_str().unwrap_or("?").to_owned())
    }
}

fn status_of(e: &anyhow::Error) -> Option<u16> {
    e.downcast_ref::<ApiError>().map(|a| a.status)
}

impl Wiki for Outline {
    async fn ensure_collection(
        &self,
        known: Option<&str>,
        name: &str,
        access: Access,
    ) -> Result<String> {
        let wanted = permission(access);
        let mut found = None;
        if let Some(id) = known {
            match self
                .call::<ApiCollection>("collections.info", json!({"id": id}))
                .await
            {
                Ok(c) if c.deleted_at.is_none() && c.archived_at.is_none() => found = Some(c),
                Ok(_) => {}
                // deleted, or no longer the bot's: looked up by its name
                Err(e) if matches!(status_of(&e), Some(400 | 403 | 404)) => {}
                Err(e) => return Err(e),
            }
        }
        // A private one only by its ID: one with that name that someone else made would keep
        // its maker as a member, who would then read the group's minutes.
        if found.is_none() && access != Access::Private {
            let listed: Vec<ApiCollection> = self
                .call("collections.list", json!({"query": name, "limit": 100}))
                .await?;
            found = listed.into_iter().find(|c| c.name.trim() == name);
        }
        if let Some(c) = found {
            let mut update = serde_json::Map::new();
            if c.permission.as_deref() != wanted {
                update.insert("permission".into(), json!(wanted));
            }
            if c.name.trim() != name {
                update.insert("name".into(), json!(name));
            }
            if !update.is_empty() {
                update.insert("id".into(), json!(c.id));
                // Outline answers 403 when the key's account is no admin of the collection
                // (one made by hand): an error, so it shows in the log until fixed.
                let _: ApiCollection = self
                    .call("collections.update", Value::Object(update))
                    .await
                    .context("setting the collection's access and name")?;
                tracing::info!(collection = %name, from = %c.name, ?wanted, "Collection access and name set");
            }
            return Ok(c.id);
        }
        let c: ApiCollection = self
            .call(
                "collections.create",
                json!({"name": name, "permission": wanted, "sharing": false}),
            )
            .await?;
        Ok(c.id)
    }

    async fn grant_group(&self, collection_id: &str, group: &str) -> Result<bool> {
        // Outline 1.11 answers {groups, groupMemberships} here, not a list (checked 2026-10-22).
        let found: GroupList = self
            .call("groups.list", json!({"query": group, "limit": 100}))
            .await?;
        let Some(g) = found.groups.into_iter().find(|g| g.name == group) else {
            return Ok(false);
        };
        // Outline keeps one membership per group and collection: adding again only sets the
        // permission.
        let _: Value = self
            .call(
                "collections.add_group",
                json!({"id": collection_id, "groupId": g.id, "permission": "read_write"}),
            )
            .await?;
        Ok(true)
    }

    async fn find_doc(&self, collection_id: &str, title: &str) -> Result<Option<Doc>> {
        // documents.list, not the title search: the search index lags behind a document that
        // was just created (seen 2026-10-20), and a miss here creates a second template.
        const PAGE: usize = 100;
        for page in 0..50 {
            let docs: Vec<ApiDoc> = self
                .call(
                    "documents.list",
                    json!({"collectionId": collection_id, "limit": PAGE, "offset": page * PAGE}),
                )
                .await?;
            let n = docs.len();
            if let Some(hit) = docs.into_iter().find(|d| {
                d.title.trim() == title && d.deleted_at.is_none() && d.archived_at.is_none()
            }) {
                return self.doc(&hit.id).await;
            }
            if n < PAGE {
                break;
            }
        }
        Ok(None)
    }

    async fn create_doc(
        &self,
        id: &str,
        collection_id: &str,
        parent_id: Option<&str>,
        title: &str,
        text: &str,
    ) -> Result<Doc> {
        let d: ApiDoc = self
            .call(
                "documents.create",
                json!({"id": id, "title": title, "text": text, "collectionId": collection_id,
                       "parentDocumentId": parent_id, "publish": true}),
            )
            .await?;
        Ok(d.into())
    }

    async fn rename(&self, id: &str, title: &str) -> Result<()> {
        let _: ApiDoc = self
            .call("documents.update", json!({"id": id, "title": title}))
            .await?;
        Ok(())
    }

    async fn replace(&self, id: &str, text: &str) -> Result<()> {
        let _: ApiDoc = self
            .call(
                "documents.update",
                json!({"id": id, "text": text, "editMode": "replace"}),
            )
            .await?;
        Ok(())
    }

    async fn doc(&self, id: &str) -> Result<Option<Doc>> {
        match self
            .call::<ApiDoc>("documents.info", json!({"id": id}))
            .await
        {
            Ok(d) if d.deleted_at.is_some() => Ok(None),
            Ok(d) => Ok(Some(d.into())),
            // Only "not found" means gone. 403 (the key lost access) is an error: closing a
            // meeting as "protocol deleted" would drop its decisions and its summary.
            Err(e) if status_of(&e) == Some(404) => Ok(None),
            Err(e) => Err(e),
        }
    }

    async fn patch(&self, id: &str, find: &str, text: &str) -> Result<bool> {
        // Outline replaces the first `findText` in the document and nothing else (checked
        // 2026-10-20: a literal "[x]" elsewhere stays as it is, unlike a full replace).
        match self
            .call::<ApiDoc>(
                "documents.update",
                json!({"id": id, "editMode": "patch", "findText": find, "text": text}),
            )
            .await
        {
            Ok(_) => Ok(true),
            // 404 here: the text is not in the document (the document itself was just read).
            Err(e) if status_of(&e) == Some(404) => Ok(false),
            Err(e) => Err(e),
        }
    }

    async fn append(&self, id: &str, text: &str) -> Result<()> {
        let _: ApiDoc = self
            .call(
                "documents.update",
                json!({"id": id, "text": text, "editMode": "append"}),
            )
            .await?;
        Ok(())
    }

    async fn attach(
        &self,
        doc_id: &str,
        name: &str,
        content_type: &str,
        bytes: Vec<u8>,
    ) -> Result<String> {
        let up: Upload = self
            .call(
                "attachments.create",
                json!({"name": name, "documentId": doc_id, "contentType": content_type,
                       "size": bytes.len()}),
            )
            .await?;
        let url = match (up.mode.as_deref(), up.upload_url) {
            (Some("post") | None, Some(u)) => u,
            (mode, _) => {
                return Err(anyhow!(
                    "Outline attachments.create: upload mode {mode:?} is not supported"
                ))
            }
        };
        let url = if url.starts_with('/') {
            format!("{}{url}", self.base)
        } else {
            url
        };
        // The key only goes to Outline itself (with S3 storage the upload goes elsewhere and is
        // authorised by the signed form fields).
        let ours = url.starts_with(&format!("{}/", self.base));
        let mut form = reqwest::multipart::Form::new();
        for (k, v) in up.form {
            let v = match v {
                Value::String(s) => s,
                other => other.to_string(),
            };
            form = form.text(k, v);
        }
        let part = reqwest::multipart::Part::bytes(bytes)
            .file_name(name.to_owned())
            .mime_str(content_type)?;
        form = form.part("file", part);
        let mut request = self.http.post(url).multipart(form);
        if ours {
            request = request.bearer_auth(&self.key);
        }
        let resp = request.send().await.context("Outline upload")?;
        if !resp.status().is_success() {
            return Err(anyhow!("Outline upload: {}", resp.status()));
        }
        Ok(up.attachment.url)
    }

    fn link(&self, path: &str) -> String {
        if path.starts_with("http://") || path.starts_with("https://") {
            path.to_owned()
        } else {
            format!("{}{path}", self.public)
        }
    }
}

/// Against a real Outline (`cargo test -- --ignored`, with `OUTLINE_IT_URL` and
/// `OUTLINE_IT_KEY`): a whole meeting, from the template to the PDF, in a new collection.
#[cfg(test)]
mod against_outline {
    use super::*;
    use crate::{
        commands,
        config::{PdfConfig, Plenum},
        engine::{fakes::FakeChat, Engine, Sender},
        state::State,
    };
    use chrono::TimeZone;
    use std::collections::HashSet;

    fn occurrence(id: &str, start: &str) -> crate::calendar::Occurrence {
        let start: crate::schedule::MeetingKey = start.parse().unwrap();
        crate::calendar::Occurrence {
            id: id.into(),
            start,
            end: Some(start.0 + chrono::Duration::hours(2)),
        }
    }

    /// A document Outline lists a moment after creating it (seen 2026-10-20: right after
    /// documents.create, documents.list and the title search may not have it yet).
    async fn listed(wiki: &Outline, collection: &str, title: &str) -> Doc {
        for _ in 0..20 {
            if let Some(d) = wiki.find_doc(collection, title).await.unwrap() {
                return d;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        panic!("{title} is not listed in Outline");
    }

    #[tokio::test]
    #[ignore = "needs a running Outline"]
    async fn a_meeting_in_a_real_outline() {
        let (Ok(url), Ok(key)) = (
            std::env::var("OUTLINE_IT_URL"),
            std::env::var("OUTLINE_IT_KEY"),
        ) else {
            panic!("set OUTLINE_IT_URL and OUTLINE_IT_KEY");
        };
        mxbot_common::logging::init("plenum_bot");
        let wiki = Outline::new(&url, &url, &key, Duration::from_secs(30)).unwrap();
        let collection = format!("IT {}", uuid::Uuid::new_v4());
        let plenum = Plenum {
            key: "house".into(),
            name: "House plenum".into(),
            room: "!house:x".try_into().unwrap(),
            collection: collection.clone(),
            calendar: "plenum-house".into(),
            prepare_days: 3,
            remind_minutes: 30,
            duration_minutes: 120,
            close_after_hours: 12,
            organizers: HashSet::new(),
            group: None,
            require_group: None,
            access: crate::config::Access::House,
        };
        let fonts = ["/usr/share/fonts/truetype/dejavu", "/usr/share/fonts/TTF"]
            .into_iter()
            .find(|d| std::path::Path::new(d).join("DejaVuSans.ttf").exists())
            .expect("DejaVu fonts")
            .to_owned();
        let dir = tempfile::tempdir().unwrap();
        let engine = Engine::new(
            vec![plenum],
            "Europe/Berlin".parse().unwrap(),
            wiki,
            FakeChat::default(),
            PdfConfig {
                enabled: true,
                font_dir: fonts,
                post_to_room: true,
            },
            HashSet::new(),
            State::default(),
            dir.path().join("state.json"),
        );
        let tz: chrono_tz::Tz = "Europe/Berlin".parse().unwrap();
        let at = |m: u32, d: u32, h: u32| {
            tz.with_ymd_and_hms(2026, m, d, h, 0, 0)
                .unwrap()
                .with_timezone(&chrono::Utc)
        };
        let room: mxbot_common::matrix_sdk::ruma::OwnedRoomId = "!house:x".try_into().unwrap();
        let anna = Sender {
            id: "@anna:x".try_into().unwrap(),
            name: "Anna".into(),
        };
        let cmd = |s: &str| commands::parse_for(s, &["house"]).unwrap();
        engine
            .apply_calendar(
                "house",
                vec![occurrence("ev#2026-11-01", "2026-11-01 19:00")],
                at(10, 1, 0),
            )
            .await;

        let a = engine
            .command(&room, &anna, cmd("!top Müll trennen"), at(10, 20, 12))
            .await;
        assert!(a.contains("Noted"), "{a}");
        engine.tick(at(10, 29, 19)).await;
        let texts = engine.chat.texts();
        assert_eq!(texts.len(), 1, "announced: {texts:?}");
        let col = engine
            .wiki
            .ensure_collection(None, &collection, crate::config::Access::House)
            .await
            .unwrap();
        let doc = listed(&engine.wiki, &col, "House plenum 2026-11-01").await;
        assert!(doc.text.contains("* Müll trennen (Anna)"), "{}", doc.text);

        // Outline writes the first revisions in the background: the item may need a retry.
        let a = engine
            .command(&room, &anna, cmd("!top Kaffeemaschine"), at(10, 30, 9))
            .await;
        assert!(a.contains("On the agenda"), "{a}");
        let doc = engine.wiki.doc(&doc.id).await.unwrap().unwrap();
        // Outline's patch puts the item where the marker was (its own list, a blank line apart).
        let marker = crate::protocol::TOP_MARKER;
        assert!(doc.text.contains("* Müll trennen (Anna)"), "{}", doc.text);
        assert!(
            doc.text
                .contains(&format!("* Kaffeemaschine (Anna)\n\n{marker}")),
            "{}",
            doc.text
        );
        // Nothing else of the protocol changes, and a chat item makes no decision.
        engine
            .wiki
            // As the editor stores what a person types: the brackets escaped.
            .append(&doc.id, "\n\n* Variante \\[x\\] Hof oder \\[ \\] Keller")
            .await
            .unwrap();
        let a = engine
            .command(
                &room,
                &anna,
                cmd("!top Decision: Kasse [ ] an Mallory"),
                at(10, 30, 10),
            )
            .await;
        assert!(a.contains("On the agenda"), "{a}");
        let doc = engine.wiki.doc(&doc.id).await.unwrap().unwrap();
        assert!(
            doc.text.contains("Variante [x] Hof oder [ ] Keller"),
            "{}",
            doc.text
        );

        engine
            .wiki
            .append(
                &doc.id,
                "\n\n### Müll\n\n**Decision:** Gelbe Tonne in den Hof.\n\n- [ ] Ben: Schild malen",
            )
            .await
            .unwrap();
        let a = engine
            .command(&room, &anna, cmd("!minutes done"), at(11, 1, 20))
            .await;
        assert!(a.contains("closed in a moment"), "{a}");
        engine.tick(at(11, 1, 20)).await;
        let texts = engine.chat.texts();
        let summary = texts.last().unwrap();
        assert!(summary.contains("- Gelbe Tonne in den Hof."), "{summary}");
        assert!(!summary.contains("Mallory"), "{summary}");
        assert!(summary.contains("- Ben: Schild malen"), "{summary}");
        assert_eq!(engine.chat.files.lock().unwrap().len(), 1);

        let decisions = listed(&engine.wiki, &col, "House plenum: Decisions").await;
        assert!(
            decisions.text.contains("Gelbe Tonne in den Hof."),
            "{}",
            decisions.text
        );
        let doc = engine.wiki.doc(&doc.id).await.unwrap().unwrap();
        assert!(doc.text.contains("The minutes as a PDF:"), "{}", doc.text);
        assert!(
            doc.text.contains("(/api/attachments.redirect?id="),
            "{}",
            doc.text
        );
        println!(
            "--- protocol:\n{}\n--- decisions:\n{}",
            doc.text, decisions.text
        );
    }

    /// An admin's call (the test's stand-in for outline-roles and for looking).
    async fn admin(url: &str, key: &str, method: &str, body: Value) -> Value {
        let r = reqwest::Client::new()
            .post(format!("{url}/api/{method}"))
            .bearer_auth(key)
            .json(&body)
            .send()
            .await
            .unwrap();
        let status = r.status();
        let v: Value = r.json().await.unwrap();
        assert!(status.is_success(), "{method}: {status} {v}");
        v["data"].clone()
    }

    /// The areas of a kitchen (everyone reads, its group writes) and of the finance group
    /// (only its group) with the bot's key as a member, as in production
    /// (`OUTLINE_IT_ADMIN_KEY`: an admin's key, which makes the group as outline-roles does).
    #[tokio::test]
    #[ignore = "needs a running Outline"]
    async fn areas_with_their_access_in_a_real_outline() {
        let (Ok(url), Ok(key), Ok(admin_key)) = (
            std::env::var("OUTLINE_IT_URL"),
            std::env::var("OUTLINE_IT_KEY"),
            std::env::var("OUTLINE_IT_ADMIN_KEY"),
        ) else {
            panic!("set OUTLINE_IT_URL, OUTLINE_IT_KEY and OUTLINE_IT_ADMIN_KEY");
        };
        mxbot_common::logging::init("plenum_bot");
        let run = uuid::Uuid::new_v4().to_string()[..8].to_owned();
        let group = format!("it-kitchen-{run}");
        admin(&url, &admin_key, "groups.create", json!({"name": group})).await;
        let wiki = Outline::new(&url, &url, &key, Duration::from_secs(30)).unwrap();
        let base = |key: &str, name: &str, room: &str| Plenum {
            key: key.into(),
            name: name.into(),
            room: room.try_into().unwrap(),
            collection: format!("IT {name} {run}"),
            calendar: "plenum-house".into(),
            prepare_days: 3,
            remind_minutes: 30,
            duration_minutes: 120,
            close_after_hours: 12,
            organizers: HashSet::new(),
            group: None,
            require_group: None,
            access: crate::config::Access::House,
        };
        let kitchen = Plenum {
            group: Some(group.clone()),
            access: crate::config::Access::Members,
            ..base("kitchen", "Küchenplenum", "!k:x")
        };
        let finance = Plenum {
            group: Some(format!("it-finance-{run}")), // not in the wiki: no grant yet
            access: crate::config::Access::Private,
            ..base("finance", "Finance plenum", "!f:x")
        };
        let dir = tempfile::tempdir().unwrap();
        let engine = Engine::new(
            vec![kitchen.clone(), finance.clone()],
            "Europe/Berlin".parse().unwrap(),
            wiki,
            FakeChat::default(),
            PdfConfig {
                enabled: false,
                font_dir: String::new(),
                post_to_room: false,
            },
            HashSet::new(),
            State::default(),
            dir.path().join("state.json"),
        );
        let tz: chrono_tz::Tz = "Europe/Berlin".parse().unwrap();
        let at = |m, d, h| {
            tz.with_ymd_and_hms(2026, m, d, h, 0, 0)
                .unwrap()
                .with_timezone(&chrono::Utc)
        };
        for p in ["kitchen", "finance"] {
            engine
                .apply_calendar(
                    p,
                    vec![occurrence(&format!("{p}#2026-11-01"), "2026-11-01 19:00")],
                    at(10, 1, 0),
                )
                .await;
        }
        engine.tick(at(10, 29, 19)).await; // the areas, and the protocols of 1.11.

        // access: as the config says (seen by an admin)
        let cols = admin(&url, &admin_key, "collections.list", json!({"limit": 100})).await;
        let col = |name: &str| {
            cols.as_array()
                .unwrap()
                .iter()
                .find(|c| c["name"] == name)
                .unwrap_or_else(|| panic!("{name} missing: {cols}"))
                .clone()
        };
        let k = col(&kitchen.collection);
        assert_eq!(k["permission"], "read", "{k}");
        // private: not even the wiki's admins see it (only the bot, which made it, and the group)
        assert!(!cols.to_string().contains(&finance.collection), "{cols}");
        let mine = admin(&url, &key, "collections.list", json!({"limit": 100})).await;
        let f = mine
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["name"] == finance.collection.as_str())
            .unwrap()
            .clone();
        assert!(f["permission"].is_null(), "{f}");
        let grants = admin(
            &url,
            &admin_key,
            "collections.group_memberships",
            json!({"id": k["id"]}),
        )
        .await;
        assert!(grants.to_string().contains(&group), "{grants}");
        assert!(grants.to_string().contains("read_write"), "{grants}");

        // the documents: overview written, protocol filed under the protocols
        let kc = k["id"].as_str().unwrap();
        let overview = listed(&engine.wiki, kc, "Küchenplenum: Overview").await;
        assert!(overview.text.contains("Next meetings"), "{}", overview.text);
        assert!(
            overview.text.contains("Küchenplenum: Rules"),
            "{}",
            overview.text
        );
        let protocols = listed(&engine.wiki, kc, "Küchenplenum: Minutes").await;
        let protocol = listed(&engine.wiki, kc, "Küchenplenum 2026-11-01").await;
        assert_eq!(protocol.parent_id.as_deref(), Some(protocols.id.as_str()));

        // an older protocol at the top is filed (the next hourly check), a move renames
        let old = engine
            .wiki
            .create_doc(
                &uuid::Uuid::new_v4().to_string(),
                kc,
                None,
                "Küchenplenum 2026-10-04",
                "alt",
            )
            .await
            .unwrap();
        {
            let mut st = engine.state.lock().await;
            let ps = st.plenums.get_mut("kitchen").unwrap();
            let mut m = crate::state::Meeting {
                doc_id: old.id.clone(),
                created: true,
                ..Default::default()
            };
            m.close.done = true;
            ps.meetings.insert("kitchen#2026-10-04".into(), m);
        }
        engine.tick(at(10, 29, 21)).await;
        let old = engine.wiki.doc(&old.id).await.unwrap().unwrap();
        assert_eq!(old.parent_id.as_deref(), Some(protocols.id.as_str()));
        // moved in the calendar: the same protocol, renamed
        engine
            .apply_calendar(
                "kitchen",
                vec![occurrence("kitchen#2026-11-01", "2026-11-08 19:00")],
                at(10, 29, 22),
            )
            .await;
        engine.tick(at(10, 29, 22)).await;
        let moved = engine.wiki.doc(&protocol.id).await.unwrap().unwrap();
        assert_eq!(moved.title, "Küchenplenum 2026-11-08");

        // the overview again, now with the new date (only rewritten when it changed)
        engine.tick(at(10, 29, 23)).await;
        let overview = engine.wiki.doc(&overview.id).await.unwrap().unwrap();
        assert!(
            overview.text.contains("8 November 2026"),
            "{}",
            overview.text
        );

        // the access is put back when someone changed it, at the next check
        admin(
            &url,
            &admin_key,
            "collections.update",
            json!({"id": kc, "permission": "read_write"}),
        )
        .await;
        engine.tick(at(10, 30, 1)).await;
        let k = admin(&url, &admin_key, "collections.info", json!({"id": kc})).await;
        assert_eq!(k["permission"], "read", "{k}");

        // renamed in the config: the same
        // collection and documents, renamed; texts nobody changed follow, edited ones stay
        let rules = listed(&engine.wiki, kc, "Küchenplenum: Rules").await;
        assert!(rules.untouched, "a new document counts as untouched");
        engine
            .wiki
            .replace(&protocols.id, "Our own words")
            .await
            .unwrap();
        let state: State = {
            let st = engine.state.lock().await;
            serde_json::from_value(serde_json::to_value(&*st).unwrap()).unwrap()
        };
        let renamed = Plenum {
            name: "Kitchen plenum".into(),
            collection: format!("IT Kitchen {run}"),
            ..kitchen.clone()
        };
        let engine = Engine::new(
            vec![renamed.clone()],
            "Europe/Berlin".parse().unwrap(),
            Outline::new(&url, &url, &key, Duration::from_secs(30)).unwrap(),
            FakeChat::default(),
            PdfConfig {
                enabled: false,
                font_dir: String::new(),
                post_to_room: false,
            },
            HashSet::new(),
            state,
            dir.path().join("state.json"),
        );
        engine.tick(at(10, 30, 3)).await;
        let k = admin(&url, &admin_key, "collections.info", json!({"id": kc})).await;
        assert_eq!(k["name"], renamed.collection.as_str(), "{k}");
        let rules_now = engine.wiki.doc(&rules.id).await.unwrap().unwrap();
        assert_eq!(rules_now.title, "Kitchen plenum: Rules");
        assert!(
            rules_now.text.contains("What applies in Kitchen plenum"),
            "{}",
            rules_now.text
        );
        let protocols_now = engine.wiki.doc(&protocols.id).await.unwrap().unwrap();
        assert_eq!(protocols_now.title, "Kitchen plenum: Minutes");
        assert_eq!(protocols_now.text.trim(), "Our own words");
        let overview = engine.wiki.doc(&overview.id).await.unwrap().unwrap();
        assert_eq!(overview.title, "Kitchen plenum: Overview");
        assert!(
            overview.text.contains("Kitchen plenum: Rules"),
            "{}",
            overview.text
        );
        assert!(engine
            .wiki
            .find_doc(kc, "Küchenplenum: Rules")
            .await
            .unwrap()
            .is_none());
    }
}
