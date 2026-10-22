//! Who is in a plenum's group, from the house's directory (its identity provider,
//! `[directory]`): every active member's Matrix accounts, their house account
//! (@<login>:<server_name>, or the localpart attribute) and the other accounts an admin
//! entered for them (the matrix_ids attribute). Read again every `refresh_minutes`; while it
//! cannot be read, nothing changes in the rooms.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result};
use mxbot_common::matrix_sdk::ruma::{OwnedUserId, UserId};
use serde::Deserialize;
use tracing::warn;

use crate::config::{DirectoryConfig, DirectoryProvider};

/// The groups the bot asked for and the Matrix accounts of their members.
pub type Groups = BTreeMap<String, BTreeSet<OwnedUserId>>;

#[derive(Deserialize, Debug)]
struct User {
    username: String,
    #[serde(default = "yes")]
    is_active: bool,
    #[serde(rename = "type", default)]
    kind: String,
    #[serde(default)]
    attributes: serde_json::Map<String, serde_json::Value>,
    #[serde(default)]
    groups_obj: Vec<Group>,
}

#[derive(Deserialize, Debug)]
struct Group {
    name: String,
}

fn yes() -> bool {
    true
}

#[derive(Deserialize)]
struct Page {
    results: Vec<User>,
    #[serde(default)]
    pagination: Option<Pagination>,
}

#[derive(Deserialize)]
struct Pagination {
    #[serde(default)]
    next: serde_json::Value,
}

fn accounts_of(user: &User, cfg: &DirectoryConfig) -> Vec<OwnedUserId> {
    let localpart = cfg
        .localpart_attribute
        .as_ref()
        .and_then(|a| user.attributes.get(a))
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .unwrap_or(&user.username)
        .to_lowercase();
    let mut out: Vec<OwnedUserId> = Vec::new();
    if let Ok(id) = UserId::parse(format!("@{localpart}:{}", cfg.server_name)) {
        out.push(id);
    }
    let listed: Vec<String> = match user.attributes.get(&cfg.matrix_ids_attribute) {
        Some(serde_json::Value::Array(items)) => items
            .iter()
            .filter_map(|v| v.as_str())
            .map(str::to_owned)
            .collect(),
        Some(serde_json::Value::String(s)) => {
            s.split([',', ' ', '\n', '\t']).map(str::to_owned).collect()
        }
        _ => Vec::new(),
    };
    for id in listed.iter().map(|s| s.trim()).filter(|s| !s.is_empty()) {
        match UserId::parse(id) {
            Ok(id) if !out.contains(&id) => out.push(id),
            Ok(_) => {}
            Err(_) => warn!(user = %user.username, "Directory: not a Matrix ID, left out"),
        }
    }
    out
}

/// The members' accounts of each of `wanted`: active people only, no service accounts.
/// A group nobody is in is missing from the answer (the bot then leaves its room alone).
fn groups_of(users: &[User], cfg: &DirectoryConfig, wanted: &BTreeSet<String>) -> Groups {
    let mut out = Groups::new();
    for user in users
        .iter()
        .filter(|u| u.is_active && !u.kind.contains("service_account"))
    {
        let accounts = accounts_of(user, cfg);
        for group in user.groups_obj.iter().filter(|g| wanted.contains(&g.name)) {
            out.entry(group.name.clone())
                .or_default()
                .extend(accounts.iter().cloned());
        }
    }
    out
}

/// Reads the directory: every active user with their groups.
pub async fn read(
    client: &reqwest::Client,
    cfg: &DirectoryConfig,
    token: &str,
    wanted: &BTreeSet<String>,
) -> Result<Groups> {
    let DirectoryProvider::Authentik = cfg.provider;
    let base = cfg.api_url.trim_end_matches('/');
    let mut users = Vec::new();
    let mut page = 1u64;
    for _ in 0..50 {
        let url = format!(
            "{base}/core/users/?is_active=true&include_groups=true&page_size=200&page={page}"
        );
        let answer: Page = client
            .get(&url)
            .bearer_auth(token)
            .send()
            .await
            .context("reading the directory")?
            .error_for_status()
            .context("reading the directory")?
            .json()
            .await
            .context("the directory's answer")?;
        users.extend(answer.results);
        match answer.pagination.and_then(|p| p.next.as_u64()) {
            Some(next) if next > page => page = next,
            _ => {
                if users.is_empty() {
                    anyhow::bail!("the directory names nobody");
                }
                return Ok(groups_of(&users, cfg, wanted));
            }
        }
    }
    anyhow::bail!("the directory has more than 50 pages of users")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> DirectoryConfig {
        DirectoryConfig {
            provider: DirectoryProvider::Authentik,
            api_url: "https://login.example.org/api/v3".into(),
            server_name: "house.org".into(),
            localpart_attribute: Some("matrix_localpart".into()),
            matrix_ids_attribute: "matrix_ids".into(),
            token_env: "DIRECTORY_TOKEN".into(),
            refresh_minutes: 15,
        }
    }

    fn users() -> Vec<User> {
        serde_json::from_value(serde_json::json!([
            {"username": "anna", "groups_obj": [{"name": "residents"}, {"name": "kitchen-floor-1"}],
             "attributes": {"matrix_ids": ["@anna:matrix.org", "not an id"]}},
            {"username": "Ben", "groups_obj": [{"name": "kitchen-floor-1"}, {"name": "finance"}],
             "attributes": {"matrix_localpart": "benny"}},
            {"username": "old", "is_active": false, "groups_obj": [{"name": "kitchen-floor-1"}]},
            {"username": "svc", "type": "service_account", "groups_obj": [{"name": "kitchen-floor-1"}]},
            {"username": "cara", "groups_obj": [{"name": "kitchen-floor-3"}]}
        ]))
        .unwrap()
    }

    fn ids(list: &[&str]) -> BTreeSet<OwnedUserId> {
        list.iter().map(|s| UserId::parse(*s).unwrap()).collect()
    }

    #[test]
    fn each_wanted_group_with_its_members_accounts() {
        let wanted: BTreeSet<String> = ["kitchen-floor-1", "finance", "kitchen-floor-4"]
            .map(String::from)
            .into();
        let g = groups_of(&users(), &cfg(), &wanted);
        assert_eq!(
            g["kitchen-floor-1"],
            ids(&["@anna:house.org", "@anna:matrix.org", "@benny:house.org"])
        );
        assert_eq!(g["finance"], ids(&["@benny:house.org"]));
        // nobody in it (or unknown): not in the answer, so its room is left alone
        assert!(!g.contains_key("kitchen-floor-4"));
        assert!(!g.contains_key("kitchen-floor-3"));
    }
}
