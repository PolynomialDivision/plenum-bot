//! The Outline API (`POST <url>/api/<method>`, JSON, `Authorization: Bearer <key>`), only
//! the calls the bot needs. Checked against Outline 1.11.0 (2026-10-20): a document can be
//! created with an ID of our own, `documents.update` with `lastRevision` answers 409 when
//! someone changed the document meanwhile, a deleted document still answers `documents.info`
//! (with `deletedAt`), and an attachment is a `attachments.create` plus a multipart upload.
//!
//! The house group (2026-10-31, subtenants): with [`Outline::with_house_group`], nothing the bot
//! makes is open to the whole workspace. Outline 1.12.0 self-hosted has no guest role (its
//! Community plan refuses it), so a subtenant is a workspace member, and a member reads every
//! collection that is open to everyone. Instead of "everyone reads/writes", the house plenum's
//! group (who lives here, an Outline group kept by house-infra's outline-roles) gets that
//! permission and the collection is private: who lives here reads and writes what they did, a
//! subtenant only what their own groups (their kitchen) are given. A house group Outline does
//! not have is never a reason to open anything: the collections stay as they are, a new one is
//! private until the group exists ([`ensure_collection`]).

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
    /// The Outline group that stands for the whole house (module docs); None: "everyone in the
    /// workspace", as before 2026-10-31.
    house_group: Option<String>,
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
struct ApiPin {
    id: String,
    #[serde(rename = "documentId")]
    document_id: String,
}

#[derive(Deserialize)]
struct PinList {
    pins: Vec<ApiPin>,
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

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ApiGroupMembership {
    group_id: String,
    permission: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GroupMemberships {
    #[serde(default)]
    group_memberships: Vec<ApiGroupMembership>,
}

/// What the whole house may do in a plenum's collection: as the permission for everyone in the
/// workspace, or (with a house group) as that group's permission on a private collection.
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
            house_group: None,
        })
    }

    /// The house plenum's group (config [`crate::config::wiki_house_group`]): the collections
    /// are private and this group gets what everyone in the workspace got before (module docs).
    pub fn with_house_group(mut self, group: Option<String>) -> Self {
        self.house_group = group.filter(|g| !g.trim().is_empty());
        self
    }

    async fn call<T: for<'de> Deserialize<'de>>(&self, method: &str, body: Value) -> Result<T> {
        let bytes = self.send(method, body).await?;
        let env: Envelope<T> = serde_json::from_slice(&bytes)
            .with_context(|| format!("Outline {method}: unexpected answer"))?;
        Ok(env.data)
    }

    /// A call whose answer has no `data` (`collections.remove_group`: `{"success": true}`).
    async fn call_ok(&self, method: &str, body: Value) -> Result<()> {
        self.send(method, body).await.map(|_| ())
    }

    /// The answer's body of a call that went through (200), or the [`ApiError`].
    async fn send(&self, method: &str, body: Value) -> Result<Vec<u8>> {
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
        Ok(bytes.to_vec())
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

/// A patch's `findText` is not in the document.
fn text_not_found(e: &anyhow::Error) -> bool {
    e.downcast_ref::<ApiError>().is_some_and(|a| {
        a.status == 404 || (a.status == 400 && a.message.to_lowercase().contains("not found"))
    })
}

/// The collection calls [`ensure_collection`] and [`grant_group`] make: Outline's API, a fake in
/// the tests (`collection_rules`).
#[allow(async_fn_in_trait)]
trait Collections {
    /// The id of the group with exactly this name, if there is one.
    async fn group_id(&self, name: &str) -> Result<Option<String>>;
    /// The collection (an [`ApiError`] for one that is gone or not the bot's).
    async fn info(&self, id: &str) -> Result<ApiCollection>;
    /// The collections whose name matches `name` (Outline's search).
    async fn list(&self, name: &str) -> Result<Vec<ApiCollection>>;
    async fn create(&self, name: &str, permission: Option<&str>) -> Result<ApiCollection>;
    /// Sets `fields` (`permission`, `name`) of the collection `id`.
    async fn update(&self, id: &str, fields: serde_json::Map<String, Value>) -> Result<()>;
    /// The group's permission on the collection; None: it has none.
    async fn group_permission(&self, collection_id: &str, group_id: &str)
        -> Result<Option<String>>;
    /// Gives the group `permission` (Outline keeps one membership per group and collection:
    /// adding again only sets the permission).
    async fn add_group(&self, collection_id: &str, group_id: &str, permission: &str) -> Result<()>;
    async fn remove_group(&self, collection_id: &str, group_id: &str) -> Result<()>;
}

impl Collections for Outline {
    async fn group_id(&self, name: &str) -> Result<Option<String>> {
        // Outline 1.11 answers {groups, groupMemberships} here, not a list (checked 2026-10-22).
        let found: GroupList = self
            .call("groups.list", json!({"query": name, "limit": 100}))
            .await?;
        Ok(found
            .groups
            .into_iter()
            .find(|g| g.name == name)
            .map(|g| g.id))
    }

    async fn info(&self, id: &str) -> Result<ApiCollection> {
        self.call("collections.info", json!({"id": id})).await
    }

    async fn list(&self, name: &str) -> Result<Vec<ApiCollection>> {
        self.call("collections.list", json!({"query": name, "limit": 100}))
            .await
    }

    async fn create(&self, name: &str, permission: Option<&str>) -> Result<ApiCollection> {
        self.call(
            "collections.create",
            json!({"name": name, "permission": permission, "sharing": false}),
        )
        .await
    }

    async fn update(&self, id: &str, mut fields: serde_json::Map<String, Value>) -> Result<()> {
        fields.insert("id".into(), json!(id));
        let _: ApiCollection = self
            .call("collections.update", Value::Object(fields))
            .await?;
        Ok(())
    }

    async fn group_permission(
        &self,
        collection_id: &str,
        group_id: &str,
    ) -> Result<Option<String>> {
        let now: GroupMemberships = self
            .call(
                "collections.group_memberships",
                json!({"id": collection_id, "limit": 100}),
            )
            .await?;
        Ok(now
            .group_memberships
            .into_iter()
            .find(|m| m.group_id == group_id)
            .map(|m| m.permission))
    }

    async fn add_group(&self, collection_id: &str, group_id: &str, permission: &str) -> Result<()> {
        self.call_ok(
            "collections.add_group",
            json!({"id": collection_id, "groupId": group_id, "permission": permission}),
        )
        .await
    }

    async fn remove_group(&self, collection_id: &str, group_id: &str) -> Result<()> {
        self.call_ok(
            "collections.remove_group",
            json!({"id": collection_id, "groupId": group_id}),
        )
        .await
    }
}

/// The house group as one collection call finds it.
enum House {
    /// None configured: the workspace's permission, as before 2026-10-31.
    Open,
    /// The group, with its Outline id.
    Found(String, String),
    /// Configured, but Outline has no such group (yet: outline-roles makes it).
    Missing(String),
}

/// What the house group gets on a plenum's collection: what everyone got before
/// ([`permission`]), and read and write when it is the plenum's own `group` too (the grant of
/// [`grant_group`]; anything less would take it back every hour).
fn house_permission(access: Access, group: Option<&str>, house: &str) -> Option<&'static str> {
    if group == Some(house) {
        Some("read_write")
    } else {
        permission(access)
    }
}

/// Gives the group `wanted` on the collection (None: takes it away), when it differs.
async fn set_group<C: Collections>(
    api: &C,
    collection_id: &str,
    group: &str,
    group_id: &str,
    wanted: Option<&str>,
) -> Result<()> {
    let has = api.group_permission(collection_id, group_id).await?;
    if has.as_deref() == wanted {
        return Ok(());
    }
    match wanted {
        Some(p) => api.add_group(collection_id, group_id, p).await,
        None => api.remove_group(collection_id, group_id).await,
    }
    .context("setting the house group's access")?;
    tracing::info!(%group, ?has, ?wanted, "The house group's access to a collection set");
    Ok(())
}

/// [`Wiki::ensure_collection`] with the house group `house_group` (module docs). With a house
/// group configured nothing is ever opened to the workspace: one Outline does not have (yet)
/// leaves an existing collection's permission as it is (a private one stays private, logged)
/// and makes a new one private (closed to the house too until the group exists; the next hourly
/// check gives it the group). A plenum made `private` is made private either way.
async fn ensure_collection<C: Collections>(
    api: &C,
    house_group: Option<&str>,
    known: Option<&str>,
    name: &str,
    access: Access,
    group: Option<&str>,
) -> Result<String> {
    let house = match house_group {
        None => House::Open,
        Some(g) => match api.group_id(g).await? {
            Some(id) => House::Found(g.to_owned(), id),
            None => House::Missing(g.to_owned()),
        },
    };
    // the permission for everyone in the workspace
    let wanted = match house {
        House::Open => permission(access),
        House::Found(..) | House::Missing(_) => None,
    };
    // Without the group the house cannot be given what it should have: no collection is
    // opened (that would widen a private one) and none is closed (that would lock out the
    // house), unless the plenum itself is private.
    let keep = matches!(house, House::Missing(_)) && access != Access::Private;
    if let (House::Missing(g), true) = (&house, keep) {
        tracing::warn!(group = %g, collection = %name, "The wiki has no such house group yet (outline-roles makes it): an existing collection's access stays as it is, a new one is private until the group exists");
    }
    let mut found = None;
    if let Some(id) = known {
        match api.info(id).await {
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
        found = api
            .list(name)
            .await?
            .into_iter()
            .find(|c| c.name.trim() == name);
    }
    if let Some(c) = found {
        // the group before the collection is made private: the house never loses it
        if let House::Found(g, id) = &house {
            set_group(api, &c.id, g, id, house_permission(access, group, g)).await?;
        }
        let mut update = serde_json::Map::new();
        if c.permission.as_deref() != wanted && !keep {
            update.insert("permission".into(), json!(wanted));
        }
        if c.name.trim() != name {
            update.insert("name".into(), json!(name));
        }
        if !update.is_empty() {
            // Outline answers 403 when the key's account is no admin of the collection
            // (one made by hand): an error, so it shows in the log until fixed.
            api.update(&c.id, update)
                .await
                .context("setting the collection's access and name")?;
            tracing::info!(collection = %name, from = %c.name, ?wanted, "Collection access and name set");
        }
        return Ok(c.id);
    }
    let c = api.create(name, wanted).await?;
    if let House::Found(g, id) = &house {
        set_group(api, &c.id, g, id, house_permission(access, group, g)).await?;
    }
    Ok(c.id)
}

/// [`Wiki::grant_group`].
async fn grant_group<C: Collections>(api: &C, collection_id: &str, group: &str) -> Result<bool> {
    let Some(group_id) = api.group_id(group).await? else {
        return Ok(false);
    };
    api.add_group(collection_id, &group_id, "read_write")
        .await?;
    Ok(true)
}

impl Wiki for Outline {
    async fn ensure_collection(
        &self,
        known: Option<&str>,
        name: &str,
        access: Access,
        group: Option<&str>,
    ) -> Result<String> {
        ensure_collection(
            self,
            self.house_group.as_deref(),
            known,
            name,
            access,
            group,
        )
        .await
    }

    async fn grant_group(&self, collection_id: &str, group: &str) -> Result<bool> {
        grant_group(self, collection_id, group).await
    }

    async fn pins(&self, collection_id: &str) -> Result<Option<Vec<(String, String)>>> {
        match self
            .call::<PinList>(
                "pins.list",
                json!({"collectionId": collection_id, "limit": 100}),
            )
            .await
        {
            Ok(l) => Ok(Some(
                l.pins.into_iter().map(|p| (p.id, p.document_id)).collect(),
            )),
            // the key has no pins.* (house-infra stacks/outline PLENUM_BOT_SCOPE)
            Err(e) if matches!(status_of(&e), Some(401 | 403)) => Ok(None),
            Err(e) => Err(e),
        }
    }

    async fn pin(&self, collection_id: &str, doc_id: &str) -> Result<bool> {
        // Outline keeps one pin per document and collection (pinCreator: findOrCreate)
        match self
            .call::<Value>(
                "pins.create",
                json!({"documentId": doc_id, "collectionId": collection_id}),
            )
            .await
        {
            Ok(_) => Ok(true),
            // no right to pin there: the bot is no admin of the collection, or the key
            Err(e) if matches!(status_of(&e), Some(401 | 403)) => Ok(false),
            Err(e) => Err(e),
        }
    }

    async fn unpin(&self, pin_id: &str) -> Result<bool> {
        match self.call_ok("pins.delete", json!({"id": pin_id})).await {
            Ok(()) => Ok(true),
            // gone already
            Err(e) if status_of(&e) == Some(404) => Ok(true),
            // no right to unpin it (a member who is no admin of the collection, or the key)
            Err(e) if matches!(status_of(&e), Some(401 | 403)) => Ok(false),
            Err(e) => Err(e),
        }
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
            // The text is not in the document (the document itself was just read): 404 before,
            // a 400 "The specified text was not found in the document" in Outline 1.12
            // (server/errors.ts, DocumentHelper).
            Err(e) if text_not_found(&e) => Ok(false),
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

/// The collection rules ([`ensure_collection`], [`grant_group`]) against a fake Outline that
/// models the groups' memberships and logs every change in order.
#[cfg(test)]
mod collection_rules {
    use super::*;
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    #[derive(Clone)]
    struct Col {
        id: String,
        name: String,
        permission: Option<String>,
        /// group id → permission
        groups: BTreeMap<String, String>,
    }

    #[derive(Default)]
    struct FakeOutline {
        /// the groups' names (the id is `g-<name>`)
        groups: Mutex<Vec<String>>,
        cols: Mutex<Vec<Col>>,
        log: Mutex<Vec<String>>,
    }

    impl FakeOutline {
        fn with_groups(names: &[&str]) -> Self {
            let wiki = Self::default();
            wiki.groups
                .lock()
                .unwrap()
                .extend(names.iter().map(|n| (*n).to_owned()));
            wiki
        }
        /// A collection someone (or the bot before) made, with the groups' permissions.
        fn add(&self, name: &str, permission: Option<&str>, groups: &[(&str, &str)]) -> String {
            let mut cols = self.cols.lock().unwrap();
            let id = format!("col-{}", cols.len());
            cols.push(Col {
                id: id.clone(),
                name: name.into(),
                permission: permission.map(Into::into),
                groups: groups
                    .iter()
                    .map(|(g, p)| (format!("g-{g}"), (*p).to_owned()))
                    .collect(),
            });
            id
        }
        fn col(&self, id: &str) -> Col {
            let cols = self.cols.lock().unwrap();
            cols.iter().find(|c| c.id == id).unwrap().clone()
        }
        /// The group's permission on the collection.
        fn group(&self, id: &str, group: &str) -> Option<String> {
            self.col(id).groups.get(&format!("g-{group}")).cloned()
        }
        /// The changes since the last call.
        fn changes(&self) -> Vec<String> {
            std::mem::take(&mut *self.log.lock().unwrap())
        }
        fn note(&self, line: String) {
            self.log.lock().unwrap().push(line);
        }
        fn with<T>(&self, id: &str, f: impl FnOnce(&mut Col) -> T) -> Result<T> {
            let mut cols = self.cols.lock().unwrap();
            let c = cols.iter_mut().find(|c| c.id == id).ok_or(ApiError {
                status: 404,
                error: "not_found".into(),
                message: String::new(),
            })?;
            Ok(f(c))
        }
    }

    fn api(c: &Col) -> ApiCollection {
        ApiCollection {
            id: c.id.clone(),
            name: c.name.clone(),
            permission: c.permission.clone(),
            deleted_at: None,
            archived_at: None,
        }
    }

    fn shown(permission: Option<&str>) -> &str {
        permission.unwrap_or("null")
    }

    impl Collections for FakeOutline {
        async fn group_id(&self, name: &str) -> Result<Option<String>> {
            let groups = self.groups.lock().unwrap();
            Ok(groups.iter().find(|g| *g == name).map(|g| format!("g-{g}")))
        }
        async fn info(&self, id: &str) -> Result<ApiCollection> {
            self.with(id, |c| api(c))
        }
        async fn list(&self, name: &str) -> Result<Vec<ApiCollection>> {
            let cols = self.cols.lock().unwrap();
            Ok(cols.iter().filter(|c| c.name == name).map(api).collect())
        }
        async fn create(&self, name: &str, permission: Option<&str>) -> Result<ApiCollection> {
            self.note(format!("create {name} permission={}", shown(permission)));
            let id = self.add(name, permission, &[]);
            Ok(api(&self.col(&id)))
        }
        async fn update(&self, id: &str, fields: serde_json::Map<String, Value>) -> Result<()> {
            let mut changed: Vec<String> = Vec::new();
            self.with(id, |c| {
                if let Some(p) = fields.get("permission") {
                    c.permission = p.as_str().map(Into::into);
                    changed.push(format!("permission={}", shown(p.as_str())));
                }
                if let Some(n) = fields.get("name") {
                    c.name = n.as_str().unwrap().into();
                    changed.push(format!("name={}", c.name));
                }
            })?;
            self.note(format!("update {id} {}", changed.join(" ")));
            Ok(())
        }
        async fn group_permission(
            &self,
            collection_id: &str,
            group_id: &str,
        ) -> Result<Option<String>> {
            self.with(collection_id, |c| c.groups.get(group_id).cloned())
        }
        async fn add_group(
            &self,
            collection_id: &str,
            group_id: &str,
            permission: &str,
        ) -> Result<()> {
            self.with(collection_id, |c| {
                c.groups.insert(group_id.into(), permission.into())
            })?;
            let group = group_id.trim_start_matches("g-");
            self.note(format!("add_group {collection_id} {group} {permission}"));
            Ok(())
        }
        async fn remove_group(&self, collection_id: &str, group_id: &str) -> Result<()> {
            self.with(collection_id, |c| c.groups.remove(group_id))?;
            let group = group_id.trim_start_matches("g-");
            self.note(format!("remove_group {collection_id} {group}"));
            Ok(())
        }
    }

    const HOUSE: Option<&str> = Some("residents");

    async fn ensure(
        wiki: &FakeOutline,
        known: Option<&str>,
        name: &str,
        access: Access,
        group: Option<&str>,
    ) -> String {
        ensure_collection(wiki, HOUSE, known, name, access, group)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn a_missing_house_group_never_opens_a_collection() {
        // made private while the group existed; the group renamed or not made yet
        let wiki = FakeOutline::with_groups(&["kitchen-floor-3"]);
        let house = wiki.add("House plenum", None, &[]);
        let kitchen = wiki.add("Kitchen", None, &[("kitchen-floor-3", "read_write")]);
        let old = wiki.add("Old plenum", Some("read_write"), &[]);
        ensure(&wiki, Some(&house), "House plenum", Access::House, None).await;
        ensure(
            &wiki,
            Some(&kitchen),
            "Kitchen",
            Access::Members,
            Some("kitchen-floor-3"),
        )
        .await;
        // found by its name too
        ensure(&wiki, None, "House plenum", Access::House, None).await;
        // an open one is not closed (that would lock out the house) either
        ensure(&wiki, Some(&old), "Old plenum", Access::House, None).await;
        assert_eq!(wiki.changes(), Vec::<String>::new());
        assert_eq!(wiki.col(&house).permission, None);
        assert_eq!(wiki.col(&kitchen).permission, None);
        assert_eq!(wiki.col(&old).permission.as_deref(), Some("read_write"));
        // a new name: only the name
        ensure(&wiki, Some(&house), "House", Access::Members, None).await;
        assert_eq!(wiki.changes(), [format!("update {house} name=House")]);
        // a new collection is private (closed to the house until the group exists)
        let fresh = ensure(&wiki, None, "Fresh", Access::House, None).await;
        assert_eq!(wiki.changes(), ["create Fresh permission=null"]);
        assert_eq!(wiki.col(&fresh).permission, None);
        // a plenum made private is made private
        ensure(&wiki, Some(&old), "Old plenum", Access::Private, None).await;
        assert_eq!(wiki.changes(), [format!("update {old} permission=null")]);
        // the group made: the house gets its access, nothing is opened
        wiki.groups.lock().unwrap().push("residents".into());
        ensure(&wiki, Some(&fresh), "Fresh", Access::House, None).await;
        assert_eq!(
            wiki.changes(),
            [format!("add_group {fresh} residents read_write")]
        );
        assert_eq!(wiki.col(&fresh).permission, None);
    }

    #[tokio::test]
    async fn the_house_group_gets_its_access_before_a_collection_is_made_private() {
        let wiki = FakeOutline::with_groups(&["residents", "kitchen-floor-3"]);
        // open to the workspace, as before the house group
        let house = wiki.add("House plenum", Some("read_write"), &[]);
        let kitchen = wiki.add(
            "Kitchen",
            Some("read"),
            &[("kitchen-floor-3", "read_write")],
        );
        ensure(&wiki, Some(&house), "House plenum", Access::House, None).await;
        assert_eq!(
            wiki.changes(),
            [
                format!("add_group {house} residents read_write"),
                format!("update {house} permission=null"),
            ]
        );
        ensure(
            &wiki,
            Some(&kitchen),
            "Kitchen",
            Access::Members,
            Some("kitchen-floor-3"),
        )
        .await;
        assert_eq!(
            wiki.changes(),
            [
                format!("add_group {kitchen} residents read"),
                format!("update {kitchen} permission=null"),
            ]
        );
        assert_eq!(
            wiki.group(&kitchen, "kitchen-floor-3").as_deref(),
            Some("read_write")
        );
        // the next check changes nothing
        ensure(&wiki, Some(&house), "House plenum", Access::House, None).await;
        ensure(
            &wiki,
            Some(&kitchen),
            "Kitchen",
            Access::Members,
            Some("kitchen-floor-3"),
        )
        .await;
        assert_eq!(wiki.changes(), Vec::<String>::new());
        // a new one: made private, then the house gets it
        let fresh = ensure(&wiki, None, "Fresh", Access::House, None).await;
        assert_eq!(
            wiki.changes(),
            [
                "create Fresh permission=null".to_owned(),
                format!("add_group {fresh} residents read_write"),
            ]
        );
        // without a house group: open as before
        let open = ensure_collection(&wiki, None, None, "Open", Access::Members, None)
            .await
            .unwrap();
        assert_eq!(wiki.changes(), ["create Open permission=read"]);
        assert_eq!(wiki.group(&open, "residents"), None);
    }

    #[tokio::test]
    async fn a_kitchen_made_private_and_back_loses_and_gets_the_house_group() {
        let wiki = FakeOutline::with_groups(&["residents", "kitchen-floor-3"]);
        let kitchen = wiki.add(
            "Kitchen",
            None,
            &[("residents", "read"), ("kitchen-floor-3", "read_write")],
        );
        let k = Some("kitchen-floor-3");
        ensure(&wiki, Some(&kitchen), "Kitchen", Access::Private, k).await;
        assert_eq!(
            wiki.changes(),
            [format!("remove_group {kitchen} residents")]
        );
        assert_eq!(wiki.group(&kitchen, "residents"), None);
        assert_eq!(
            wiki.group(&kitchen, "kitchen-floor-3").as_deref(),
            Some("read_write")
        );
        ensure(&wiki, Some(&kitchen), "Kitchen", Access::Members, k).await;
        assert_eq!(
            wiki.changes(),
            [format!("add_group {kitchen} residents read")]
        );
        // members → house → members: the house's permission follows
        ensure(&wiki, Some(&kitchen), "Kitchen", Access::House, k).await;
        ensure(&wiki, Some(&kitchen), "Kitchen", Access::Members, k).await;
        assert_eq!(
            wiki.changes(),
            [
                format!("add_group {kitchen} residents read_write"),
                format!("add_group {kitchen} residents read"),
            ]
        );
        assert_eq!(wiki.col(&kitchen).permission, None);
        // an open one made private: the house group goes (it had none), then the collection
        let open = wiki.add("Open kitchen", Some("read"), &[("residents", "read")]);
        ensure(&wiki, Some(&open), "Open kitchen", Access::Private, k).await;
        assert_eq!(
            wiki.changes(),
            [
                format!("remove_group {open} residents"),
                format!("update {open} permission=null"),
            ]
        );
    }

    #[tokio::test]
    async fn a_plenum_whose_group_is_the_house_group_keeps_writing() {
        let wiki = FakeOutline::with_groups(&["residents"]);
        for access in [Access::Members, Access::Private] {
            let id = wiki.add(&format!("{access:?}"), None, &[]);
            // hourly: the collection, then the plenum's group (as engine.rs setup() does)
            for round in 0..3 {
                ensure(&wiki, Some(&id), &format!("{access:?}"), access, HOUSE).await;
                assert!(grant_group(&wiki, &id, "residents").await.unwrap());
                assert_eq!(
                    wiki.group(&id, "residents").as_deref(),
                    Some("read_write"),
                    "{access:?} {round}"
                );
            }
            // never taken down to read or away, only given read and write again
            let changes = wiki.changes();
            assert!(
                changes
                    .iter()
                    .all(|c| c == &format!("add_group {id} residents read_write")),
                "{access:?}: {changes:?}"
            );
            assert_eq!(changes.len(), 4, "{changes:?}");
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

    #[test]
    fn a_missing_text_is_not_found_in_both_outline_versions() {
        let e = |status: u16, message: &str| -> anyhow::Error {
            ApiError {
                status,
                error: String::new(),
                message: message.into(),
            }
            .into()
        };
        assert!(text_not_found(&e(404, "")));
        assert!(text_not_found(&e(
            400,
            "The specified text was not found in the document"
        )));
        assert!(!text_not_found(&e(400, "text: Required")));
        assert!(!text_not_found(&e(403, "not found")));
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
            publication: crate::config::Publication::Full,
            messages: crate::config::Messages::All,
            room_settings: true,
            page_after_days: None,
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
            .ensure_collection(None, &collection, crate::config::Access::House, None)
            .await
            .unwrap();
        let doc = listed(&engine.wiki, &col, "House plenum 2026-11-01").await;
        assert!(
            doc.text
                .contains("### Müll trennen\n\n**From:** Anna (from the chat)\n\n**Result:**"),
            "{}",
            doc.text
        );

        // Outline writes the first revisions in the background: the item may need a retry.
        let a = engine
            .command(&room, &anna, cmd("!top Kaffeemaschine"), at(10, 30, 9))
            .await;
        assert!(a.contains("On the page"), "{a}");
        let doc = engine.wiki.doc(&doc.id).await.unwrap().unwrap();
        // Outline's patch puts the topic above the marker, which stays
        let marker = crate::page::TOPIC_MARKER;
        assert!(doc.text.contains("### Müll trennen"), "{}", doc.text);
        assert!(
            doc.text.contains(&format!(
                "### Kaffeemaschine\n\n**From:** Anna (from the chat)\n\n**Result:**\n\n{marker}"
            )),
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
        assert!(a.contains("On the page"), "{a}");
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
            // at its end: before it only an organizer closes the minutes
            .command(&room, &anna, cmd("!minutes done"), at(11, 1, 21))
            .await;
        assert!(a.contains("closed in a moment"), "{a}");
        engine.tick(at(11, 1, 21)).await; // the result box
        engine.tick(at(11, 1, 22)).await; // seen there: closed
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

    /// A page's text as Outline gives it back, printed (its normalisations are what this is
    /// about).
    async fn page_text(wiki: &Outline, id: &str, step: &str) -> String {
        let text = wiki.doc(id).await.unwrap().expect("the page").text;
        eprintln!("--- {step}:\n{text}\n---");
        text
    }

    /// A meeting's page (docs/PLENUM-PAGES.md) as Outline stores it: the template, topics from
    /// the chat, a result written by hand, the result box and a carried topic, each patched in
    /// as the engine does and read back by the bot's page functions.
    #[tokio::test]
    #[ignore = "needs a running Outline"]
    async fn a_page_in_a_real_outline() {
        use crate::{
            page::{self, Reading},
            protocol::{self, Outcome},
            schedule::MeetingKey,
            state::Top,
        };
        let (Ok(url), Ok(key)) = (
            std::env::var("OUTLINE_IT_URL"),
            std::env::var("OUTLINE_IT_KEY"),
        ) else {
            panic!("set OUTLINE_IT_URL and OUTLINE_IT_KEY");
        };
        let wiki = Outline::new(&url, &url, &key, Duration::from_secs(30)).unwrap();
        let col = wiki
            .ensure_collection(
                None,
                &format!("IT {}", uuid::Uuid::new_v4()),
                Access::House,
                None,
            )
            .await
            .unwrap();
        let plenum = protocol::tests_plenum();
        let new_page = |start: &str, without_marker: bool| {
            let start: MeetingKey = start.parse().unwrap();
            let mut text =
                protocol::render_template(protocol::DEFAULT_TEMPLATE, &plenum, start, &[]);
            if without_marker {
                text = text.replace(page::TOPIC_MARKER, "");
            }
            (protocol::title(&plenum, start), text)
        };

        // 1. the template
        let (title, text) = new_page("2026-11-01 19:00", false);
        let id = uuid::Uuid::new_v4().to_string();
        let doc = wiki
            .create_doc(&id, &col, None, &title, &text)
            .await
            .unwrap();
        let text = page_text(&wiki, &id, "template").await;
        assert!(page::has_topics(&text));
        assert!(page::topics(&text).is_empty());
        assert_eq!(protocol::extract(&text), Outcome::default());
        assert!(text.contains(page::TOPIC_MARKER), "{text}");
        assert!(text.contains(page::RESULT_MARKER), "{text}");

        // 2. two topics from the chat, as `!top` adds them
        let tops = [
            Top {
                text: "Fahrräder im Hof\nWo sollen die Räder hin?".into(),
                by: "Ben".into(),
                at: chrono::Utc::now(),
            },
            Top {
                text: "Kasse *neu* _jetzt_ [x] #1 ➕ Plan\nWir brauchen eine neue Kasse.\n\
                       Decision: Mallory bekommt den Schlüssel\nResult: schon klar"
                    .into(),
                by: "Anna [x](http://evil)".into(),
                at: chrono::Utc::now(),
            },
        ];
        let tricky = "Kasse *neu* _jetzt_ [x] #1 + Plan";
        for top in &tops {
            let block = page::topic_block(top);
            eprintln!("--- block sent:\n{block}\n---");
            let marker = page::TOPIC_MARKER;
            assert!(wiki
                .patch(&id, marker, &format!("{block}\n\n{marker}"))
                .await
                .unwrap());
            let text = wiki.doc(&id).await.unwrap().unwrap().text;
            // what add_top_to looks for after a restart
            let head = block.split("\n\n").take(2).collect::<Vec<_>>().join("\n\n");
            assert!(protocol::contains_line(&text, &head), "{head}\n{text}");
        }
        let text = page_text(&wiki, &id, "two topics").await;
        let topics = page::topics(&text);
        let titles: Vec<String> = topics.iter().map(|t| t.title()).collect();
        assert_eq!(titles, ["Fahrräder im Hof", tricky], "{topics:?}");
        assert!(
            topics.iter().all(|t| t.reading == Reading::NotReached),
            "{topics:?}"
        );
        assert_eq!(protocol::extract(&text), Outcome::default());
        assert!(text.contains(page::TOPIC_MARKER), "{text}");

        // 3. the minute-taker writes the first topic's result, with a Shift-Enter break
        let at = text.find("Fahrräder im Hof").unwrap();
        let result = text[at..]
            .lines()
            .find(|l| l.replace('*', "").trim() == "Result:")
            .expect("the first topic's result line")
            .to_owned();
        eprintln!("--- result line as Outline gives it: {result:?}");
        assert!(wiki
            .patch(
                &id,
                &result,
                &format!(
                    "{result} Sie kommen an die Mauer.\\\nZweite Zeile nach Shift-Enter\n\n\
                     Decision: Anna kauft einen Ständer bis Dezember."
                ),
            )
            .await
            .unwrap());
        let text = page_text(&wiki, &id, "first result").await;
        let lines = page::lines(&text);
        assert!(
            lines
                .iter()
                .any(|l| l.trim() == "Zweite Zeile nach Shift-Enter"),
            "{lines:#?}"
        );
        assert!(
            lines
                .iter()
                .any(|l| l.contains("Sie kommen an die Mauer.") && !l.contains("Zweite")),
            "{lines:#?}"
        );
        let topics = page::topics(&text);
        let readings: Vec<Reading> = topics.iter().map(|t| t.reading).collect();
        assert_eq!(
            readings,
            [Reading::Decided, Reading::NotReached],
            "{topics:?}"
        );
        let outcome = protocol::extract(&text);
        assert_eq!(
            outcome.decisions,
            ["Anna kauft einen Ständer bis Dezember."],
            "{outcome:?}"
        );

        // 4. the result box at the top
        let result_box = page::result_box(&outcome.decisions, &topics, &outcome.tasks);
        eprintln!("--- box sent:\n{result_box}\n---");
        assert!(wiki
            .patch(&id, page::RESULT_MARKER, &format!("\n\n{result_box}\n\n"))
            .await
            .unwrap());
        let text = page_text(&wiki, &id, "with the box").await;
        assert!(page::has_box(&text), "{text}");
        assert!(!page::has_box(&page::without_box(&text)), "{text}");
        assert!(!text.contains(page::RESULT_MARKER), "{text}");
        assert_eq!(protocol::extract(&text), outcome);
        assert_eq!(page::topics(&text), topics);

        // 5. the topic not reached onto the next pages: at the marker, and above "Tasks"
        let carried = &topics[1];
        assert_eq!(carried.title(), tricky);
        let block = page::carried_block(
            carried,
            chrono::NaiveDate::from_ymd_opt(2026, 11, 1).unwrap(),
            &wiki.link(&doc.url),
        );
        eprintln!("--- carried block sent:\n{block}\n---");
        for (start, without_marker) in [("2026-11-15 19:00", false), ("2026-11-29 19:00", true)] {
            let (title, text) = new_page(start, without_marker);
            let next = uuid::Uuid::new_v4().to_string();
            wiki.create_doc(&next, &col, None, &title, &text)
                .await
                .unwrap();
            let text = page_text(&wiki, &next, &format!("{title} before")).await;
            let find = if without_marker {
                page::end_of_topics(&text)
                    .expect("the Tasks heading")
                    .to_owned()
            } else {
                page::TOPIC_MARKER.to_owned()
            };
            assert!(wiki
                .patch(&next, &find, &format!("{block}\n\n{find}"))
                .await
                .unwrap());
            let text = page_text(&wiki, &next, &format!("{title} carried")).await;
            let topics = page::topics(&text);
            assert_eq!(topics.len(), 1, "{topics:?}");
            assert_eq!(topics[0].title(), tricky, "{topics:?}");
            assert_eq!(topics[0].reading, Reading::NotReached, "{topics:?}");
            // what the engine checks before it drops a carried topic from its state
            assert!(
                page::has_heading(&text, page::block_heading(&block)),
                "{}\n{text}",
                page::block_heading(&block)
            );
            assert_eq!(protocol::extract(&text), Outcome::default());
        }

        // 6. a text that is not there
        assert!(!wiki
            .patch(&id, "Diesen Text gibt es nicht", "x")
            .await
            .unwrap());
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
            publication: crate::config::Publication::Full,
            messages: crate::config::Messages::All,
            room_settings: true,
            page_after_days: None,
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
    /// The house group (2026-10-31): the house plenum, a kitchen and the finance group as the
    /// engine sets them up (`ensure_collection`, then `grant_group` for a plenum with a group
    /// that is not the house's), first without it (open to the workspace, as before), then with
    /// it: every collection private, the house group with what everyone had. Needs the groups
    /// in Outline (`OUTLINE_IT_HOUSE_GROUP`, `OUTLINE_IT_KITCHEN_GROUP`,
    /// `OUTLINE_IT_FINANCE_GROUP`, as outline-roles makes them); `OUTLINE_IT_PREFIX` names the
    /// collections ("<prefix>House plenum", ...). It leaves them set up, for a look at what a
    /// subtenant sees.
    #[tokio::test]
    #[ignore = "needs a running Outline"]
    async fn house_group_areas_in_a_real_outline() {
        let var = |k: &str| std::env::var(k).unwrap_or_else(|_| panic!("set {k}"));
        let (url, key, admin_key) = (
            var("OUTLINE_IT_URL"),
            var("OUTLINE_IT_KEY"),
            var("OUTLINE_IT_ADMIN_KEY"),
        );
        let (house_group, kitchen_group, finance_group) = (
            var("OUTLINE_IT_HOUSE_GROUP"),
            var("OUTLINE_IT_KITCHEN_GROUP"),
            var("OUTLINE_IT_FINANCE_GROUP"),
        );
        let prefix = std::env::var("OUTLINE_IT_PREFIX")
            .unwrap_or_else(|_| format!("IT {} ", &uuid::Uuid::new_v4().to_string()[..8]));
        mxbot_common::logging::init("plenum_bot");
        let areas = [
            (format!("{prefix}House plenum"), Access::House, None),
            (
                format!("{prefix}Kitchen 1st floor"),
                Access::Members,
                Some(kitchen_group.as_str()),
            ),
            (
                format!("{prefix}Finance group"),
                Access::Private,
                Some(finance_group.as_str()),
            ),
        ];
        // as engine.rs setup() does it
        async fn setup(
            wiki: &Outline,
            known: Option<&str>,
            name: &str,
            access: Access,
            group: Option<&str>,
        ) -> String {
            let id = wiki
                .ensure_collection(known, name, access, group)
                .await
                .unwrap();
            if let (Some(g), true) = (group, access != Access::House) {
                assert!(wiki.grant_group(&id, g).await.unwrap(), "no group {g}");
            }
            id
        }
        // (group name, permission) of a collection, sorted
        async fn groups_of(url: &str, admin_key: &str, id: &str) -> Vec<(String, String)> {
            let v = admin(
                url,
                admin_key,
                "collections.group_memberships",
                json!({"id": id, "limit": 100}),
            )
            .await;
            let names: std::collections::HashMap<&str, &str> = v["groups"]
                .as_array()
                .unwrap()
                .iter()
                .map(|g| (g["id"].as_str().unwrap(), g["name"].as_str().unwrap()))
                .collect();
            let mut out: Vec<(String, String)> = v["groupMemberships"]
                .as_array()
                .unwrap()
                .iter()
                .map(|m| {
                    (
                        names[m["groupId"].as_str().unwrap()].to_owned(),
                        m["permission"].as_str().unwrap().to_owned(),
                    )
                })
                .collect();
            out.sort();
            out
        }
        async fn permission_of(url: &str, admin_key: &str, id: &str) -> Value {
            admin(url, admin_key, "collections.info", json!({"id": id})).await["permission"].clone()
        }

        // before: open to the workspace
        let legacy = Outline::new(&url, &url, &key, Duration::from_secs(30)).unwrap();
        let mut ids = Vec::new();
        for (name, access, group) in &areas {
            ids.push(setup(&legacy, None, name, *access, *group).await);
        }
        assert_eq!(permission_of(&url, &admin_key, &ids[0]).await, "read_write");
        assert_eq!(permission_of(&url, &admin_key, &ids[1]).await, "read");

        // with the house group: private, the group has what everyone had
        let wiki = Outline::new(&url, &url, &key, Duration::from_secs(30))
            .unwrap()
            .with_house_group(Some(house_group.clone()));
        for _ in 0..2 {
            // twice: the second changes nothing
            for ((name, access, group), id) in areas.iter().zip(&ids) {
                let again = setup(&wiki, Some(id), name, *access, *group).await;
                assert_eq!(&again, id);
            }
        }
        let rw = |g: &str| (g.to_owned(), "read_write".to_owned());
        let ro = |g: &str| (g.to_owned(), "read".to_owned());
        for id in &ids[..2] {
            assert!(
                permission_of(&url, &admin_key, id).await.is_null(),
                "private now: {id}"
            );
        }
        assert_eq!(
            groups_of(&url, &admin_key, &ids[0]).await,
            [rw(&house_group)]
        );
        let mut kitchen = vec![ro(&house_group), rw(&kitchen_group)];
        kitchen.sort();
        assert_eq!(groups_of(&url, &admin_key, &ids[1]).await, kitchen);
        assert_eq!(
            groups_of(&url, &admin_key, &ids[2]).await,
            [rw(&finance_group)]
        );

        // a kitchen made private: the house loses it; back to members: the house reads again
        let (k, kitchen_name) = (ids[1].clone(), areas[1].0.clone());
        setup(
            &wiki,
            Some(&k),
            &kitchen_name,
            Access::Private,
            Some(&kitchen_group),
        )
        .await;
        assert_eq!(groups_of(&url, &admin_key, &k).await, [rw(&kitchen_group)]);
        setup(
            &wiki,
            Some(&k),
            &kitchen_name,
            Access::Members,
            Some(&kitchen_group),
        )
        .await;
        assert_eq!(groups_of(&url, &admin_key, &k).await, kitchen);

        // a new collection is made private with the group at once
        let fresh = setup(&wiki, None, &format!("{prefix}New"), Access::House, None).await;
        assert!(permission_of(&url, &admin_key, &fresh).await.is_null());
        assert_eq!(
            groups_of(&url, &admin_key, &fresh).await,
            [rw(&house_group)]
        );

        // without such a group in Outline: never opened to the workspace; an existing
        // collection stays as it is, a new one is private (closed to the house until the group
        // exists)
        let missing = Outline::new(&url, &url, &key, Duration::from_secs(30))
            .unwrap()
            .with_house_group(Some("no-such-group-in-outline".into()));
        let closed = setup(
            &missing,
            None,
            &format!("{prefix}Closed"),
            Access::House,
            None,
        )
        .await;
        assert!(permission_of(&url, &admin_key, &closed).await.is_null());
        setup(&missing, Some(&ids[0]), &areas[0].0, Access::House, None).await;
        assert!(permission_of(&url, &admin_key, &ids[0]).await.is_null());
        assert_eq!(
            groups_of(&url, &admin_key, &ids[0]).await,
            [rw(&house_group)]
        );
        admin(&url, &key, "collections.delete", json!({"id": closed})).await;
        println!("collections: {ids:?} new {fresh} closed {closed}");
    }

    async fn pins(w: &Outline, col: &str) -> Option<Vec<(String, String)>> {
        w.pins(col).await.unwrap()
    }

    /// The pins of a collection (the open meeting's page, engine `sync_pins`) as Outline 1.12
    /// answers them: one pin per page however often it is pinned, a second unpin is 404 (gone).
    /// Optional: `OUTLINE_IT_NOPINS_KEY`, a key whose scope has no `/api/pins.*` (the bot sees
    /// no pins and may not pin), and `OUTLINE_IT_MEMBER_KEY`, a key of a member (no admin) who
    /// writes in a collection someone else made (reads the pins, may not pin or unpin).
    #[tokio::test]
    #[ignore = "needs a running Outline"]
    async fn pins_in_a_real_outline() {
        let (Ok(url), Ok(key)) = (
            std::env::var("OUTLINE_IT_URL"),
            std::env::var("OUTLINE_IT_KEY"),
        ) else {
            panic!("set OUTLINE_IT_URL and OUTLINE_IT_KEY");
        };
        let wiki = Outline::new(&url, &url, &key, Duration::from_secs(30)).unwrap();
        let col = wiki
            .ensure_collection(
                None,
                &format!("IT {}", uuid::Uuid::new_v4()),
                Access::House,
                None,
            )
            .await
            .unwrap();
        let (a, b) = (
            uuid::Uuid::new_v4().to_string(),
            uuid::Uuid::new_v4().to_string(),
        );
        wiki.create_doc(&a, &col, None, "Pin A", "a").await.unwrap();
        wiki.create_doc(&b, &col, None, "Pin B", "b").await.unwrap();
        assert_eq!(pins(&wiki, &col).await, Some(vec![]));

        // pinned twice: still one pin
        assert!(wiki.pin(&col, &a).await.unwrap());
        assert!(wiki.pin(&col, &a).await.unwrap());
        assert!(wiki.pin(&col, &b).await.unwrap());
        let listed = pins(&wiki, &col).await.expect("the key may list pins");
        println!("pins: {listed:?}");
        assert_eq!(listed.len(), 2, "{listed:?}");
        let pin_of = |doc: &str| {
            listed
                .iter()
                .find(|(_, d)| d == doc)
                .unwrap_or_else(|| panic!("{doc} is not pinned: {listed:?}"))
                .0
                .clone()
        };
        let (pin_a, pin_b) = (pin_of(&a), pin_of(&b));

        // unpinned, and again: gone (404), also for a pin that never was
        wiki.unpin(&pin_a).await.unwrap();
        assert_eq!(
            pins(&wiki, &col).await,
            Some(vec![(pin_b.clone(), b.clone())])
        );
        let again = wiki
            .call_ok("pins.delete", json!({"id": pin_a}))
            .await
            .unwrap_err();
        assert_eq!(status_of(&again), Some(404), "{again:#}");
        wiki.unpin(&pin_a).await.unwrap();
        wiki.unpin(&uuid::Uuid::new_v4().to_string()).await.unwrap();

        // a key without pins.* in its scope: 403, no pins and no pinning for the bot
        if let Ok(nopins) = std::env::var("OUTLINE_IT_NOPINS_KEY") {
            let w = Outline::new(&url, &url, &nopins, Duration::from_secs(30)).unwrap();
            for (method, body) in [
                ("pins.list", json!({"collectionId": col, "limit": 100})),
                ("pins.create", json!({"documentId": a, "collectionId": col})),
            ] {
                let e = w.call_ok(method, body).await.unwrap_err();
                println!("without pins.*: {method}: {e:#}");
                assert_eq!(status_of(&e), Some(403), "{method}: {e:#}");
            }
            assert_eq!(pins(&w, &col).await, None);
            assert!(!w.pin(&col, &a).await.unwrap());
        } else {
            println!("OUTLINE_IT_NOPINS_KEY not set: the scoped key is not checked");
        }

        // a member who writes but is no admin of the collection: sees the pins, may not pin
        // (403 → Ok(false)); nor unpin (403 → Ok(false))
        if let Ok(member) = std::env::var("OUTLINE_IT_MEMBER_KEY") {
            let w = Outline::new(&url, &url, &member, Duration::from_secs(30)).unwrap();
            assert_eq!(pins(&w, &col).await, Some(vec![(pin_b.clone(), b.clone())]));
            let e = w
                .call_ok("pins.create", json!({"documentId": a, "collectionId": col}))
                .await
                .unwrap_err();
            println!("member: pins.create: {e:#}");
            assert_eq!(status_of(&e), Some(403), "{e:#}");
            assert!(!w.pin(&col, &a).await.unwrap());
            let e = w
                .call_ok("pins.delete", json!({"id": pin_b}))
                .await
                .unwrap_err();
            println!("member: pins.delete: {e:#}");
            assert_eq!(status_of(&e), Some(403), "{e:#}");
            assert!(!w.unpin(&pin_b).await.unwrap());
            assert_eq!(
                pins(&wiki, &col).await,
                Some(vec![(pin_b.clone(), b.clone())])
            );
        } else {
            println!("OUTLINE_IT_MEMBER_KEY not set: the member is not checked");
        }

        wiki.unpin(&pin_b).await.unwrap();
        assert_eq!(pins(&wiki, &col).await, Some(vec![]));
        admin(&url, &key, "collections.delete", json!({"id": col})).await;
    }
}
