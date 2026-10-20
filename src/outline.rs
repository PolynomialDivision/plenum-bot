//! The Outline API (`POST <url>/api/<method>`, JSON, `Authorization: Bearer <key>`), only
//! the calls the bot needs. Checked against Outline 1.11.0 (2026-10-20): a document can be
//! created with an ID of our own, `documents.update` with `lastRevision` answers 409 when
//! someone changed the document meanwhile, a deleted document still answers `documents.info`
//! (with `deletedAt`), and an attachment is a `attachments.create` plus a multipart upload.

use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::engine::{Doc, Wiki};

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
    revision: i64,
    #[serde(default)]
    collection_id: Option<String>,
    #[serde(default)]
    deleted_at: Option<String>,
    #[serde(default)]
    archived_at: Option<String>,
}

impl From<ApiDoc> for Doc {
    fn from(d: ApiDoc) -> Self {
        Doc {
            id: d.id,
            title: d.title,
            text: d.text,
            url: d.url,
            revision: d.revision,
            collection_id: d.collection_id,
        }
    }
}

#[derive(Deserialize)]
struct ApiCollection {
    id: String,
    name: String,
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
    async fn ensure_collection(&self, name: &str) -> Result<String> {
        let found: Vec<ApiCollection> = self
            .call("collections.list", json!({"query": name, "limit": 100}))
            .await?;
        if let Some(c) = found.into_iter().find(|c| c.name.trim() == name) {
            return Ok(c.id);
        }
        let c: ApiCollection = self
            .call(
                "collections.create",
                json!({"name": name, "permission": "read_write", "sharing": false}),
            )
            .await?;
        Ok(c.id)
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
        title: &str,
        text: &str,
    ) -> Result<Doc> {
        let d: ApiDoc = self
            .call(
                "documents.create",
                json!({"id": id, "title": title, "text": text,
                       "collectionId": collection_id, "publish": true}),
            )
            .await?;
        Ok(d.into())
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
        schedule::Recurrence,
        state::State,
    };
    use chrono::TimeZone;
    use std::collections::HashSet;

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
            key: "haus".into(),
            name: "Hausplenum".into(),
            room: "!haus:x".try_into().unwrap(),
            collection: collection.clone(),
            recurrence: Some(
                toml::from_str::<Recurrence>(
                    "kind = 'monthly_nth'\nweekday = 'sun'\nnths = [1]\ntime = '19:00'",
                )
                .unwrap(),
            ),
            prepare_days: 3,
            remind_minutes: 30,
            duration_minutes: 120,
            close_after_hours: 12,
            organizers: HashSet::new(),
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
        let room: mxbot_common::matrix_sdk::ruma::OwnedRoomId = "!haus:x".try_into().unwrap();
        let anna = Sender {
            id: "@anna:x".try_into().unwrap(),
            name: "Anna".into(),
        };
        let today = chrono::NaiveDate::from_ymd_opt(2026, 10, 20).unwrap();
        let cmd = |s: &str| commands::parse_for(s, today, &["haus"]).unwrap();

        let a = engine
            .command(&room, &anna, cmd("!top Müll trennen"), at(10, 20, 12))
            .await;
        assert!(a.contains("Notiert"), "{a}");
        engine.tick(at(10, 29, 19)).await;
        let texts = engine.chat.texts();
        assert_eq!(texts.len(), 1, "announced: {texts:?}");
        let col = engine.wiki.ensure_collection(&collection).await.unwrap();
        let doc = listed(&engine.wiki, &col, "Hausplenum 2026-11-01").await;
        assert!(doc.text.contains("* Müll trennen (Anna)"), "{}", doc.text);

        // Outline writes the first revisions in the background: the item may need a retry.
        let a = engine
            .command(&room, &anna, cmd("!top Kaffeemaschine"), at(10, 30, 9))
            .await;
        assert!(a.contains("Steht auf der Tagesordnung"), "{a}");
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
                cmd("!top Beschluss: Kasse [ ] an Mallory"),
                at(10, 30, 10),
            )
            .await;
        assert!(a.contains("Steht auf der Tagesordnung"), "{a}");
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
                "\n\n### Müll\n\n**Beschluss:** Gelbe Tonne in den Hof.\n\n- [ ] Ben: Schild malen",
            )
            .await
            .unwrap();
        let a = engine
            .command(&room, &anna, cmd("!protokoll fertig"), at(11, 1, 20))
            .await;
        assert!(a.contains("wird gleich abgeschlossen"), "{a}");
        engine.tick(at(11, 1, 20)).await;
        let texts = engine.chat.texts();
        let summary = texts.last().unwrap();
        assert!(summary.contains("- Gelbe Tonne in den Hof."), "{summary}");
        assert!(!summary.contains("Mallory"), "{summary}");
        assert!(summary.contains("- Ben: Schild malen"), "{summary}");
        assert_eq!(engine.chat.files.lock().unwrap().len(), 1);

        let decisions = listed(&engine.wiki, &col, "Beschlüsse Hausplenum").await;
        assert!(
            decisions.text.contains("Gelbe Tonne in den Hof."),
            "{}",
            decisions.text
        );
        let doc = engine.wiki.doc(&doc.id).await.unwrap().unwrap();
        assert!(doc.text.contains("Das Protokoll als PDF:"), "{}", doc.text);
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
}
