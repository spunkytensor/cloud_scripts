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
/// Confirms that GitHub rejects the credential, requesting revocation if still valid.
pub async fn revoke(token: &str) -> Result<()> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| Error::Uncertain(format!("GitHub revocation client: {e}")))?;
    revoke_at(&client, "https://api.github.com", token).await
}

/// Keeps accepted revocation requests uncertain until the credential is rejected.
async fn revoke_at(client: &reqwest::Client, api: &str, token: &str) -> Result<()> {
    // A retry may encounter a token already revoked or expired. Do not resubmit it.
    if credential_invalid(client, api, token).await? {
        return Ok(());
    }
    let r = client
        .post(format!("{api}/credentials/revoke"))
        .header("Accept", "application/vnd.github+json")
        .header("User-Agent", "vps-control-plane")
        .json(&serde_json::json!({"credentials":[token]}))
        .send()
        .await
        .map_err(|e| Error::Uncertain(format!("GitHub revocation: {e}")))?;
    if !r.status().is_success() {
        return Err(Error::Uncertain(format!(
            "GitHub revocation returned HTTP {}",
            r.status()
        )));
    }
    if credential_invalid(client, api, token).await? {
        Ok(())
    } else {
        Err(Error::Uncertain(
            "GitHub accepted revocation but the credential remains valid; local evidence retained, retry after revocation completes".into(),
        ))
    }
}

/// Only an authentication rejection proves invalidity; forbidden or unavailable is uncertain.
async fn credential_invalid(client: &reqwest::Client, api: &str, token: &str) -> Result<bool> {
    let response = client
        .get(format!("{api}/user"))
        .bearer_auth(token)
        .header("Accept", "application/vnd.github+json")
        .header("User-Agent", "vps-control-plane")
        .send()
        .await
        .map_err(|e| Error::Uncertain(format!("GitHub credential confirmation: {e}")))?;
    match response.status() {
        reqwest::StatusCode::UNAUTHORIZED => Ok(true),
        reqwest::StatusCode::OK => Ok(false),
        status => Err(Error::Uncertain(format!(
            "GitHub credential confirmation returned HTTP {status}; local evidence retained"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serves only synthetic credentials over loopback, checking each request in order.
    fn github_server(
        replies: Vec<(&'static str, Option<u16>)>,
    ) -> (String, std::thread::JoinHandle<()>) {
        use std::io::{BufRead, Read, Write};
        use std::time::{Duration, Instant};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let api = format!("http://{}", listener.local_addr().unwrap());
        let worker = std::thread::spawn(move || {
            for (request_line, status) in replies {
                let deadline = Instant::now() + Duration::from_secs(5);
                let mut stream = loop {
                    match listener.accept() {
                        Ok((stream, _)) => break stream,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            assert!(Instant::now() < deadline, "missing {request_line}");
                            std::thread::sleep(Duration::from_millis(5));
                        }
                        Err(error) => panic!("{error}"),
                    }
                };
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut reader = std::io::BufReader::new(&stream);
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                assert_eq!(line.trim(), request_line);
                let mut length = 0;
                let mut authorized = false;
                loop {
                    line.clear();
                    assert_ne!(reader.read_line(&mut line).unwrap(), 0);
                    if line == "\r\n" {
                        break;
                    }
                    let header = line.to_ascii_lowercase();
                    if let Some(value) = header.strip_prefix("content-length:") {
                        length = value.trim().parse().unwrap();
                    }
                    authorized |= header.trim() == "authorization: bearer github_pat_fixture";
                }
                let mut body = vec![0; length];
                reader.read_exact(&mut body).unwrap();
                if request_line.starts_with("GET") {
                    assert!(authorized);
                } else {
                    assert!(!authorized);
                    assert_eq!(
                        serde_json::from_slice::<serde_json::Value>(&body).unwrap(),
                        serde_json::json!({"credentials": ["github_pat_fixture"]})
                    );
                }
                if let Some(status) = status {
                    write!(
                        stream,
                        "HTTP/1.1 {status} Test\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    )
                    .unwrap();
                }
            }
        });
        (api, worker)
    }

    /// An accepted request is not proof, and retries can confirm asynchronous invalidation.
    #[tokio::test]
    async fn revocation_requires_rejection_and_can_retry() {
        let (api, server) = github_server(vec![
            ("GET /user HTTP/1.1", Some(200)),
            ("POST /credentials/revoke HTTP/1.1", Some(202)),
            ("GET /user HTTP/1.1", Some(200)),
            ("GET /user HTTP/1.1", Some(401)),
        ]);
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let error = revoke_at(&client, &api, "github_pat_fixture")
            .await
            .unwrap_err();
        assert!(matches!(error, Error::Uncertain(_)));
        assert!(!error.to_string().contains("github_pat_fixture"));
        revoke_at(&client, &api, "github_pat_fixture")
            .await
            .unwrap();
        server.join().unwrap();
    }

    /// Only 401 confirms invalidity; forbidden, throttled, missing, and lost responses do not.
    #[tokio::test]
    async fn revocation_confirmation_is_conservative() {
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        for status in [Some(401), Some(403), Some(404), Some(429), Some(500), None] {
            let (api, server) = github_server(vec![
                ("GET /user HTTP/1.1", Some(200)),
                ("POST /credentials/revoke HTTP/1.1", Some(202)),
                ("GET /user HTTP/1.1", status),
            ]);
            let result = revoke_at(&client, &api, "github_pat_fixture").await;
            if status == Some(401) {
                result.unwrap();
            } else {
                assert!(matches!(result, Err(Error::Uncertain(_))), "{status:?}");
            }
            server.join().unwrap();
        }
    }

    /// An invalid token needs no submission, while inconclusive probes cannot report success.
    #[tokio::test]
    async fn revocation_initial_probe_is_conservative() {
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        for status in [Some(401), Some(403), Some(429), Some(500), None] {
            let (api, server) = github_server(vec![("GET /user HTTP/1.1", status)]);
            let result = revoke_at(&client, &api, "github_pat_fixture").await;
            if status == Some(401) {
                result.unwrap();
            } else {
                assert!(matches!(result, Err(Error::Uncertain(_))), "{status:?}");
            }
            server.join().unwrap();
        }
    }

    /// A listening endpoint that never replies must not hang teardown or confirm invalidity.
    #[tokio::test]
    async fn revocation_probe_timeout_is_uncertain() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let client = reqwest::Client::builder()
            .no_proxy()
            .timeout(std::time::Duration::from_millis(50))
            .build()
            .unwrap();
        assert!(matches!(
            revoke_at(
                &client,
                &format!("http://{}", listener.local_addr().unwrap()),
                "github_pat_fixture"
            )
            .await,
            Err(Error::Uncertain(_))
        ));
    }

    /// Failed submissions retain uncertainty rather than claiming revocation.
    #[tokio::test]
    async fn revocation_submission_failure_is_uncertain() {
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        for status in [Some(401), Some(403), Some(429), Some(503), None] {
            let (api, server) = github_server(vec![
                ("GET /user HTTP/1.1", Some(200)),
                ("POST /credentials/revoke HTTP/1.1", status),
            ]);
            assert!(matches!(
                revoke_at(&client, &api, "github_pat_fixture").await,
                Err(Error::Uncertain(_))
            ));
            server.join().unwrap();
        }
    }

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
