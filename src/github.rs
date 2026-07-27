// SPDX-FileCopyrightText: 2026 Matt Curfman
// SPDX-License-Identifier: Apache-2.0

use crate::error::{Error, Result};
use std::{fs, path::Path};
#[derive(Debug, Clone)]
pub struct Replacement {
    pub old: String,
    pub new: String,
}
/// Builds the GitHub fine-grained token creation URL scoped to the repository owner.
pub fn creation_url(repository: &str, server_id: &str) -> String {
    let mut url = reqwest::Url::parse("https://github.com/settings/personal-access-tokens/new")
        .expect("static GitHub URL is valid");
    let owner = repository
        .split_once('/')
        .map_or(repository, |(owner, _)| owner);
    url.query_pairs_mut()
        .append_pair("name", &format!("Codex VPS {server_id}"))
        .append_pair(
            "description",
            &format!("Temporary autonomous worker for {repository}"),
        )
        .append_pair("target_name", owner)
        .append_pair("expires_in", "2")
        .append_pair("contents", "write")
        .append_pair("pull_requests", "write")
        .append_pair("actions", "read")
        .append_pair("statuses", "read");
    url.into()
}
/// Validates this value's persisted invariants and rejects unsafe or contradictory state.
pub async fn validate(token: &str, repository: &str) -> Result<String> {
    if !token.starts_with("github_pat_") {
        return Err(Error::Cli(
            "expected a fine-grained PAT beginning with github_pat_".into(),
        ));
    }
    let client = reqwest::Client::new();
    let get = |path: String| {
        client
            .get(format!("https://api.github.com/{path}"))
            .bearer_auth(token)
            .header("Accept", "application/vnd.github+json")
            .header("User-Agent", "vps-control-plane")
    };
    let user = get("user".into())
        .send()
        .await
        .map_err(|e| Error::Remote(format!("GitHub API: {e}")))?;
    if !user.status().is_success() {
        return if matches!(user.status().as_u16(), 401 | 403) {
            Err(Error::Cli("GitHub PAT does not identify a user".into()))
        } else {
            Err(Error::Remote(format!(
                "GitHub user validation returned HTTP {}",
                user.status()
            )))
        };
    }
    let login = user
        .json::<serde_json::Value>()
        .await
        .map_err(|e| Error::Remote(e.to_string()))?["login"]
        .as_str()
        .unwrap_or("")
        .to_owned();
    if login.trim().is_empty() {
        return Err(Error::Remote(
            "GitHub user validation returned no login".into(),
        ));
    }
    let repo = get(format!("repos/{repository}"))
        .send()
        .await
        .map_err(|e| Error::Remote(format!("GitHub API: {e}")))?;
    if !repo.status().is_success() {
        return Err(Error::Cli(format!("GitHub PAT cannot access {repository}")));
    }
    Ok(login)
}
/// Reads the retained credential token, if present, without logging its contents.
pub fn retained_token(dir: &Path) -> Result<Option<String>> {
    let p = dir.join("github-token");
    if !p.exists() {
        return Ok(None);
    }
    let t = fs::read_to_string(p)?;
    if !t.trim().starts_with("github_pat_") {
        return Err(Error::State("retained GitHub token is malformed".into()));
    }
    Ok(Some(t.trim().into()))
}
/// Reads and deduplicates all retained credential tokens eligible for revocation.
pub fn retained_tokens(dir: &Path) -> Result<Vec<String>> {
    let mut values = Vec::new();
    if let Some(v) = retained_token(dir)? {
        values.push(v);
    }
    let journal = dir.join("github-token.replacement");
    if journal.exists() {
        let fields = crate::state::legacy::parse(&fs::read_to_string(journal)?)?;
        for key in [
            "REPLACEMENT_OLD_GITHUB_TOKEN",
            "REPLACEMENT_NEW_GITHUB_TOKEN",
        ] {
            let value = fields
                .get(key)
                .ok_or_else(|| Error::State(format!("replacement journal field {key} missing")))?;
            if !value.starts_with("github_pat_") {
                return Err(Error::State(
                    "replacement journal token is malformed".into(),
                ));
            }
            if !values.contains(value) {
                values.push(value.clone());
            }
        }
    }
    Ok(values)
}
/// Loads a pending credential replacement only when both old and new token records are complete.
pub fn replacement(dir: &Path) -> Result<Option<Replacement>> {
    let journal = dir.join("github-token.replacement");
    if !journal.exists() {
        return Ok(None);
    }
    let fields = crate::state::legacy::parse(&fs::read_to_string(journal)?)?;
    let get = |key: &str| -> Result<String> {
        let value = fields
            .get(key)
            .ok_or_else(|| Error::State(format!("replacement journal field {key} missing")))?;
        if !value.starts_with("github_pat_") {
            return Err(Error::State(
                "replacement journal token is malformed".into(),
            ));
        }
        Ok(value.clone())
    };
    Ok(Some(Replacement {
        old: get("REPLACEMENT_OLD_GITHUB_TOKEN")?,
        new: get("REPLACEMENT_NEW_GITHUB_TOKEN")?,
    }))
    .and_then(|replacement| {
        if replacement.as_ref().is_some_and(|r| r.old == r.new) {
            Err(Error::State(
                "replacement journal contains identical old and new tokens".into(),
            ))
        } else {
            Ok(replacement)
        }
    })
}

/// Encodes a credential replacement record for private atomic persistence.
pub fn replacement_bytes(old: &str, new: &str) -> Result<Vec<u8>> {
    // Fine-grained PATs are deliberately restricted to this non-shell-active alphabet.
    if old == new
        || ![old, new].iter().all(|v| {
            v.starts_with("github_pat_")
                && v.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
        })
    {
        return Err(Error::State(
            "replacement journal token is malformed".into(),
        ));
    }
    Ok(
        format!("REPLACEMENT_OLD_GITHUB_TOKEN={old}\nREPLACEMENT_NEW_GITHUB_TOKEN={new}\n")
            .into_bytes(),
    )
}
/// Requests GitHub token revocation and rejects responses that do not confirm success.
pub async fn revoke(token: &str) -> Result<()> {
    let r = reqwest::Client::new()
        .post("https://api.github.com/credentials/revoke")
        .header("Accept", "application/vnd.github+json")
        .header("User-Agent", "vps-control-plane")
        .json(&serde_json::json!({"credentials":[token]}))
        .send()
        .await
        .map_err(|e| Error::Uncertain(format!("GitHub revocation: {e}")))?;
    if r.status().is_success() {
        Ok(())
    } else {
        Err(Error::Uncertain(format!(
            "GitHub revocation returned HTTP {}",
            r.status()
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The PAT creation URL targets the repository owner and requests only required permissions.
    #[test]
    fn creation_url_scopes_token_to_repository_owner() {
        let url = reqwest::Url::parse(&creation_url("owner/repo name", "123")).unwrap();
        let query: std::collections::HashMap<_, _> = url.query_pairs().collect();
        assert_eq!(query["name"], "Codex VPS 123");
        assert_eq!(
            query["description"],
            "Temporary autonomous worker for owner/repo name"
        );
        assert_eq!(query["target_name"], "owner");
        assert_eq!(query["expires_in"], "2");
        assert_eq!(query["contents"], "write");
        assert_eq!(query["pull_requests"], "write");
        assert_eq!(query["actions"], "read");
        assert_eq!(query["statuses"], "read");
    }
}
