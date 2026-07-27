use super::{Backend, Mutation};
use crate::{
    config::DigitalOcean as Settings,
    error::{Error, Result},
    model::*,
};
use async_trait::async_trait;
use reqwest::{Client, Method, StatusCode};
use serde_json::{Value, json};
use std::{env, time::Duration};

pub struct DigitalOcean {
    client: Client,
    base: String,
    token: String,
}
impl DigitalOcean {
    pub fn new(s: &Settings) -> Result<Self> {
        let token = env::var("DIGITALOCEAN_TOKEN")
            .or_else(|_| env::var("DOCTL_TOKEN"))
            .map_err(|_| {
                Error::Cli("DIGITALOCEAN_TOKEN is required (DOCTL_TOKEN is deprecated)".into())
            })?;
        Ok(Self {
            client: Client::builder()
                .timeout(Duration::from_secs(30))
                .build()
                .map_err(|e| Error::Backend(e.to_string()))?,
            base: s
                .api_url
                .clone()
                .unwrap_or_else(|| "https://api.digitalocean.com/v2".into()),
            token,
        })
    }
    async fn req(&self, m: Method, path: &str, body: Option<Value>) -> Result<Option<Value>> {
        self.req_url(m, format!("{}{}", self.base, path), body)
            .await
    }
    async fn req_url(&self, m: Method, url: String, body: Option<Value>) -> Result<Option<Value>> {
        let mut r = self
            .client
            .request(m.clone(), url)
            .bearer_auth(&self.token)
            .header("Accept", "application/json");
        if let Some(b) = body {
            r = r.json(&b)
        }
        let x = r.send().await.map_err(|e| {
            if m == Method::GET {
                Error::Backend(e.to_string())
            } else {
                Error::Uncertain(e.to_string())
            }
        })?;
        let status = x.status();
        if status == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let request = x
            .headers()
            .get("x-request-id")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("unknown")
            .to_owned();
        let bytes = x.bytes().await.map_err(|e| {
            if m == Method::GET {
                Error::Backend(e.to_string())
            } else {
                Error::Uncertain(e.to_string())
            }
        })?;
        if !status.is_success() {
            let msg = serde_json::from_slice::<Value>(&bytes)
                .ok()
                .and_then(|v| v["message"].as_str().map(str::to_owned))
                .unwrap_or_else(|| status.to_string());
            let diagnostic = format!("HTTP {status}, request {request}: {msg}");
            return Err(if m != Method::GET && mutation_status_uncertain(status) {
                Error::Uncertain(diagnostic)
            } else {
                Error::Backend(diagnostic)
            });
        }
        if bytes.is_empty() {
            Ok(Some(Value::Null))
        } else {
            serde_json::from_slice(&bytes).map(Some).map_err(|e| {
                if m == Method::GET {
                    Error::Json(e)
                } else {
                    Error::Uncertain(format!("successful mutation response was malformed: {e}"))
                }
            })
        }
    }

    async fn pages(&self, first: String, field: &str) -> Result<Vec<Value>> {
        let base = reqwest::Url::parse(&self.base).map_err(|e| Error::Backend(e.to_string()))?;
        let mut next = Some(format!("{}{}", self.base, first));
        let mut items = Vec::new();
        while let Some(url) = next.take() {
            let page = self
                .req_url(Method::GET, url, None)
                .await?
                .unwrap_or_default();
            items.extend(page[field].as_array().cloned().unwrap_or_default());
            next = page
                .pointer("/links/pages/next")
                .and_then(Value::as_str)
                .map(|raw| {
                    reqwest::Url::parse(raw).and_then(|candidate| {
                        if candidate.scheme() != base.scheme()
                            || candidate.host_str() != base.host_str()
                            || candidate.port_or_known_default() != base.port_or_known_default()
                            || !candidate.path().starts_with(base.path())
                        {
                            return Err(url::ParseError::RelativeUrlWithoutBase);
                        }
                        Ok(candidate.to_string())
                    })
                })
                .transpose()
                .map_err(|_| Error::Backend("provider returned an unsafe pagination URL".into()))?;
        }
        Ok(items)
    }
}
fn mutation_status_uncertain(status: StatusCode) -> bool {
    status == StatusCode::REQUEST_TIMEOUT
        || status == StatusCode::TOO_MANY_REQUESTS
        || status.is_server_error()
}
fn server(v: &Value) -> Result<Server> {
    let d = &v["droplet"];
    let endpoint = d["networks"]["v4"]
        .as_array()
        .and_then(|a| a.iter().find(|x| x["type"] == "public"))
        .and_then(|x| x["ip_address"].as_str())
        .map(str::to_owned);
    Ok(Server {
        id: id(&d["id"])?,
        name: text(d, "name")?,
        endpoint,
        region: text(&d["region"], "slug")?,
        size: text(&d["size"], "slug")?,
        image: text(&d["image"], "slug").or_else(|_| id(&d["image"]["id"]))?,
        tags: d["tags"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default(),
        disk_gb: d["disk"].as_u64().unwrap_or(0),
        status: d["status"].as_str().unwrap_or_default().to_owned(),
        volume_ids: d["volume_ids"]
            .as_array()
            .map(|a| a.iter().filter_map(|x| id(x).ok()).collect())
            .unwrap_or_default(),
    })
}
fn id(v: &Value) -> Result<String> {
    v.as_str()
        .map(str::to_owned)
        .or_else(|| v.as_u64().map(|x| x.to_string()))
        .ok_or_else(|| Error::Backend("response is missing resource ID".into()))
}
fn text(v: &Value, k: &str) -> Result<String> {
    v[k].as_str()
        .map(str::to_owned)
        .ok_or_else(|| Error::Backend(format!("response is missing {k}")))
}
#[async_trait]
impl Backend for DigitalOcean {
    async fn validate_access(&self) -> Result<()> {
        // Probe a permission required by the lifecycle rather than requiring
        // the unrelated account:read scope from least-privilege tokens.
        self.req(Method::GET, "/droplets?per_page=1", None)
            .await?
            .ok_or_else(|| Error::Backend("droplet access check returned no response".into()))
            .map(|_| ())
    }
    async fn create_server(
        &self,
        r: &CreateRecipe,
        user_data: Option<&str>,
    ) -> Result<Mutation<Server>> {
        match self.req(Method::POST,"/droplets",Some(json!({"name":r.name,"region":r.region,"size":r.size,"image":r.image,"ssh_keys":[r.ssh_key],"tags":r.tags,"user_data":user_data,"monitoring":true}))).await {
            Ok(Some(v)) => match server(&v) {
                Ok(server) => Ok(Mutation::Confirmed(server)),
                Err(error) => Ok(Mutation::Uncertain { diagnostic: error.to_string() }),
            },
            Ok(None) => Ok(Mutation::Uncertain { diagnostic: "empty or missing create response".into() }),
            Err(Error::Uncertain(diagnostic)) => Ok(Mutation::Uncertain { diagnostic }),
            Err(Error::Backend(diagnostic)) => Ok(Mutation::Rejected { diagnostic }),
            Err(e) => Err(e),
        }
    }
    async fn find_servers(&self, tag: &str) -> Result<Vec<Server>> {
        self.pages(format!("/droplets?tag_name={tag}"), "droplets")
            .await?
            .iter()
            .map(|d| server(&json!({"droplet":d})))
            .collect()
    }
    async fn get_server(&self, id_: &str) -> Result<Option<Server>> {
        match self
            .req(Method::GET, &format!("/droplets/{id_}"), None)
            .await?
        {
            Some(v) => Ok(Some(server(&v)?)),
            None => Ok(None),
        }
    }
    async fn delete_server(&self, id: &str) -> Result<Mutation<()>> {
        mutation_unit(
            self.req(Method::DELETE, &format!("/droplets/{id}"), None)
                .await,
            format!("server {id} is missing; confirm account and absence"),
        )
    }
    async fn action(
        &self,
        resource_id: &str,
        kind: &str,
        name: Option<&str>,
    ) -> Result<Mutation<Action>> {
        let mut b = json!({"type":kind});
        if let Some(n) = name {
            b["name"] = json!(n)
        }
        let result = self
            .req(
                Method::POST,
                &format!("/droplets/{resource_id}/actions"),
                Some(b),
            )
            .await;
        match result {
            Ok(Some(v)) => match action(&v["action"]) {
                Ok(action) => Ok(Mutation::Confirmed(action)),
                Err(error) => Ok(Mutation::Uncertain {
                    diagnostic: error.to_string(),
                }),
            },
            Ok(None) => Ok(Mutation::Uncertain {
                diagnostic: "empty action response".into(),
            }),
            Err(Error::Uncertain(diagnostic)) => Ok(Mutation::Uncertain { diagnostic }),
            Err(Error::Backend(diagnostic)) => Ok(Mutation::Rejected { diagnostic }),
            Err(e) => Err(e),
        }
    }
    async fn get_action(&self, id_: &str) -> Result<Option<Action>> {
        self.req(Method::GET, &format!("/actions/{id_}"), None)
            .await?
            .map(|v| action(&v["action"]))
            .transpose()
    }
    async fn find_actions(
        &self,
        resource_id: &str,
        kind: &str,
        since: &str,
    ) -> Result<Vec<Action>> {
        self.pages(format!("/droplets/{resource_id}/actions"), "actions")
            .await?
            .iter()
            .map(action)
            .filter(|a| {
                a.as_ref().map_or(true, |a| {
                    a.kind == kind
                        && a.resource_id == resource_id
                        && action_started_since(a.started_at.as_deref(), since)
                })
            })
            .collect()
    }
    async fn wait_action(&self, expected: &Action) -> Result<()> {
        for _ in 0..120 {
            let found = self
                .get_action(&expected.id)
                .await?
                .ok_or_else(|| Error::Uncertain(format!("action {} missing", expected.id)))?;
            if found.kind != expected.kind || found.resource_id != expected.resource_id {
                return Err(Error::State("provider action identity mismatch".into()));
            }
            match found.status {
                ActionStatus::Completed => return Ok(()),
                ActionStatus::Errored => {
                    return Err(Error::Backend(format!("action {} failed", expected.id)));
                }
                _ => tokio::time::sleep(Duration::from_secs(2)).await,
            }
        }
        Err(Error::Uncertain(format!(
            "action {} timed out",
            expected.id
        )))
    }
    async fn snapshots(&self) -> Result<Vec<Snapshot>> {
        self.pages("/snapshots?resource_type=droplet".into(), "snapshots")
            .await?
            .iter()
            .map(snapshot)
            .collect()
    }
    async fn get_snapshot(&self, id_: &str) -> Result<Option<Snapshot>> {
        match self
            .req(Method::GET, &format!("/snapshots/{id_}"), None)
            .await?
        {
            Some(v) => Ok(Some(snapshot(&v["snapshot"])?)),
            None => Ok(None),
        }
    }
    async fn delete_snapshot(&self, id: &str) -> Result<Mutation<()>> {
        mutation_unit(
            self.req(Method::DELETE, &format!("/snapshots/{id}"), None)
                .await,
            format!("snapshot {id} is missing; confirm account and absence"),
        )
    }
}
fn action(v: &Value) -> Result<Action> {
    Ok(Action {
        id: id(&v["id"])?,
        kind: text(v, "type")?,
        resource_id: id(&v["resource_id"])?,
        status: match v["status"].as_str() {
            Some("completed") => ActionStatus::Completed,
            Some("errored") => ActionStatus::Errored,
            _ => ActionStatus::InProgress,
        },
        started_at: v["started_at"].as_str().map(str::to_owned),
    })
}
fn action_started_since(started_at: Option<&str>, since: &str) -> bool {
    let Some(started_at) = started_at else {
        return false;
    };
    if let Ok(since) = since.parse::<i64>() {
        return rfc3339_unix_seconds(started_at).is_some_and(|started| started >= since);
    }
    started_at >= since
}
fn rfc3339_unix_seconds(value: &str) -> Option<i64> {
    let (date, time) = value.split_once('T')?;
    let mut date = date.split('-').map(|part| part.parse::<i64>().ok());
    let year = date.next()??;
    let month = date.next()??;
    let day = date.next()??;
    if date.next().is_some() || !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let time = time.strip_suffix('Z')?;
    let mut time = time.split(':');
    let hour = time.next()?.parse::<i64>().ok()?;
    let minute = time.next()?.parse::<i64>().ok()?;
    let second = time.next()?.split('.').next()?.parse::<i64>().ok()?;
    if time.next().is_some() || hour > 23 || minute > 59 || second > 60 {
        return None;
    }

    // Howard Hinnant's civil-date conversion, offset to the Unix epoch.
    let adjusted_year = year - i64::from(month <= 2);
    let era = if adjusted_year >= 0 {
        adjusted_year
    } else {
        adjusted_year - 399
    } / 400;
    let year_of_era = adjusted_year - era * 400;
    let adjusted_month = month + if month > 2 { -3 } else { 9 };
    let day_of_year = (153 * adjusted_month + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    let days = era * 146_097 + day_of_era - 719_468;
    Some(days * 86_400 + hour * 3_600 + minute * 60 + second)
}
fn mutation_unit(r: Result<Option<Value>>, missing: String) -> Result<Mutation<()>> {
    match r {
        Ok(Some(_)) => Ok(Mutation::Confirmed(())),
        Ok(None) => Ok(Mutation::Uncertain {
            diagnostic: missing,
        }),
        Err(Error::Uncertain(diagnostic)) => Ok(Mutation::Uncertain { diagnostic }),
        Err(Error::Backend(diagnostic)) => Ok(Mutation::Rejected { diagnostic }),
        Err(e) => Err(e),
    }
}
fn snapshot(v: &Value) -> Result<Snapshot> {
    let regions: Vec<String> = v["regions"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    Ok(Snapshot {
        id: id(&v["id"])?,
        name: text(v, "name")?,
        source_id: id(&v["resource_id"])?,
        region: regions.first().cloned().unwrap_or_default(),
        min_disk_gb: v["min_disk_size"].as_u64().unwrap_or(0),
        host_key: String::new(),
        pause_operation_id: String::new(),
        source_recipe: None,
        regions,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mutation_http_classification_is_conservative() {
        for status in [408, 429, 500, 503] {
            assert!(mutation_status_uncertain(
                StatusCode::from_u16(status).unwrap()
            ));
        }
        for status in [400, 401, 403, 404, 409, 422] {
            assert!(!mutation_status_uncertain(
                StatusCode::from_u16(status).unwrap()
            ));
        }
    }

    #[test]
    fn action_time_comparison_supports_persisted_unix_timestamps() {
        assert!(action_started_since(
            Some("2026-07-26T23:43:08Z"),
            "1785109387"
        ));
        assert!(!action_started_since(
            Some("2026-07-26T23:43:06Z"),
            "1785109387"
        ));
        assert!(action_started_since(
            Some("2026-07-26T23:43:08.123Z"),
            "2026-07-26T23:43:07Z"
        ));
    }
}
