use crate::error::{Error, Result};
use std::{fs, path::Path};
#[derive(Debug, Clone)]
pub struct Replacement {
    pub old: String,
    pub new: String,
}
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
