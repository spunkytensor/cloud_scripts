use crate::error::{Error, Result};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use std::{
    env, fs,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub version: u8,
    #[serde(default = "default_backend")]
    pub default_backend: String,
    pub state_dir: Option<PathBuf>,
    #[serde(default)]
    pub ssh: Ssh,
    #[serde(default)]
    pub git: Git,
    #[serde(default)]
    pub backends: Backends,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Ssh {
    pub private_key: Option<PathBuf>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Git {
    #[serde(default = "author")]
    pub author_name: String,
    #[serde(default = "email")]
    pub author_email: String,
}
impl Default for Git {
    fn default() -> Self {
        Self {
            author_name: author(),
            author_email: email(),
        }
    }
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Backends {
    #[serde(default)]
    pub digitalocean: DigitalOcean,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DigitalOcean {
    pub ssh_key: Option<String>,
    #[serde(default = "region")]
    pub region: String,
    #[serde(default = "size")]
    pub size: String,
    #[serde(default = "image")]
    pub image: String,
    #[serde(default = "tags")]
    pub tags: Vec<String>,
    #[serde(default = "prefix")]
    pub name_prefix: String,
    pub api_url: Option<String>,
}
impl Default for DigitalOcean {
    fn default() -> Self {
        Self {
            ssh_key: None,
            region: region(),
            size: size(),
            image: image(),
            tags: tags(),
            name_prefix: prefix(),
            api_url: None,
        }
    }
}
fn default_backend() -> String {
    "digitalocean".into()
}
fn region() -> String {
    "nyc3".into()
}
fn size() -> String {
    "s-4vcpu-8gb".into()
}
fn image() -> String {
    "ubuntu-24-04-x64".into()
}
fn prefix() -> String {
    "codex-agent".into()
}
fn tags() -> Vec<String> {
    vec!["codex-agent".into()]
}
fn author() -> String {
    "Codex VPS Agent".into()
}
fn email() -> String {
    "codex-vps-agent@users.noreply.github.com".into()
}
impl Default for Config {
    fn default() -> Self {
        Self {
            version: 1,
            default_backend: default_backend(),
            state_dir: None,
            ssh: Ssh::default(),
            git: Git::default(),
            backends: Backends::default(),
        }
    }
}

impl Config {
    pub fn load(path: Option<&Path>, state_override: Option<PathBuf>) -> Result<(Self, PathBuf)> {
        let mut c = if let Some(p) = path {
            let text = fs::read_to_string(p)?;
            toml::from_str(&text)?
        } else {
            Config::default()
        };
        if c.version != 1 {
            return Err(Error::Cli(format!(
                "unsupported configuration version {}",
                c.version
            )));
        };
        let d = &mut c.backends.digitalocean;
        if let Ok(v) = env::var("DO_SSH_KEY") {
            d.ssh_key = Some(v)
        }
        if let Ok(v) = env::var("DO_REGION") {
            d.region = v
        }
        if let Ok(v) = env::var("DO_SIZE") {
            d.size = v
        }
        if let Ok(v) = env::var("DO_IMAGE") {
            d.image = v
        }
        if let Ok(v) = env::var("DO_TAGS") {
            d.tags = v
                .split(',')
                .map(str::trim)
                .map(str::to_owned)
                .filter(|x| !x.is_empty())
                .collect()
        }
        if let Ok(v) = env::var("DROPLET_NAME_PREFIX") {
            d.name_prefix = v
        }
        if let Ok(v) = env::var("SSH_PRIVATE_KEY_FILE") {
            c.ssh.private_key = Some(PathBuf::from(v))
        }
        let root = state_override
            .or_else(|| c.state_dir.clone())
            .unwrap_or_else(default_state);
        Ok((c, expand(root)))
    }
}
fn default_state() -> PathBuf {
    let legacy = PathBuf::from(".state/instances");
    if legacy.exists() {
        legacy
    } else {
        ProjectDirs::from("dev", "vps", "vps")
            .and_then(|p| p.state_dir().map(|d| d.join("instances")))
            .unwrap_or(legacy)
    }
}
fn expand(p: PathBuf) -> PathBuf {
    let s = p.to_string_lossy();
    if (s == "~" || s.starts_with("~/"))
        && let Some(h) = env::var_os("HOME")
    {
        return PathBuf::from(h).join(s.trim_start_matches("~/"));
    }
    p
}

pub fn migrate_legacy(source: &Path) -> Result<String> {
    let values = crate::state::legacy::parse_file(source)?;
    let mut c = Config::default();
    let d = &mut c.backends.digitalocean;
    if let Some(v) = values.get("DO_SSH_KEY") {
        d.ssh_key = Some(v.clone())
    }
    if let Some(v) = values.get("DO_REGION") {
        d.region = v.clone()
    }
    if let Some(v) = values.get("DO_SIZE") {
        d.size = v.clone()
    }
    if let Some(v) = values.get("DO_IMAGE") {
        d.image = v.clone()
    }
    if let Some(v) = values.get("DO_TAGS") {
        d.tags = v.split(',').map(str::to_owned).collect()
    }
    if let Some(v) = values.get("DROPLET_NAME_PREFIX") {
        d.name_prefix = v.clone()
    }
    if let Some(v) = values.get("SSH_PRIVATE_KEY_FILE") {
        c.ssh.private_key = Some(PathBuf::from(v))
    }
    toml::to_string_pretty(&c).map_err(|e| Error::Cli(e.to_string()))
}
