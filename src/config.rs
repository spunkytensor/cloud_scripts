// SPDX-FileCopyrightText: 2026 Matt Curfman
// SPDX-License-Identifier: Apache-2.0

use crate::{
    console,
    error::{Error, Result},
};
use directories::BaseDirs;
use serde::{Deserialize, Serialize};
use std::{
    env,
    fs::{self, File, OpenOptions},
    io::{self, BufRead, IsTerminal, Write},
    path::{Path, PathBuf},
    process::Command,
};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub version: u8,
    #[serde(default = "default_backend")]
    pub default_backend: String,
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
    /// Supplies the worker Git identity used when the `[git]` section is omitted.
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
    /// Supplies first-run DigitalOcean region, size, image, tags, and name defaults.
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
/// Returns the default backend used when no explicit value is configured.
fn default_backend() -> String {
    "digitalocean".into()
}
/// Returns the default DigitalOcean region used by first-run configuration.
fn region() -> String {
    "nyc3".into()
}
/// Returns the default DigitalOcean Droplet size.
fn size() -> String {
    "s-4vcpu-8gb".into()
}
/// Returns the default Ubuntu image slug.
fn image() -> String {
    "ubuntu-24-04-x64".into()
}
/// Returns the default prefix for generated Droplet names.
fn prefix() -> String {
    "codex-agent".into()
}
/// Returns the default tags attached to managed Droplets.
fn tags() -> Vec<String> {
    vec!["codex-agent".into()]
}
/// Returns the default Git author name configured on workers.
fn author() -> String {
    "Codex VPS Agent".into()
}
/// Returns the default Git author email configured on workers.
fn email() -> String {
    "codex-vps-agent@users.noreply.github.com".into()
}
impl Default for Config {
    /// Builds schema-version-one configuration from section defaults.
    fn default() -> Self {
        Self {
            version: 1,
            default_backend: default_backend(),
            ssh: Ssh::default(),
            git: Git::default(),
            backends: Backends::default(),
        }
    }
}

impl Config {
    /// Loads version-one TOML configuration, applying environment overrides and tilde expansion.
    /// Missing default configuration triggers interactive first-run setup; an explicit missing path fails.
    pub fn load(path: Option<&Path>, home_override: Option<PathBuf>) -> Result<(Self, PathBuf)> {
        let home = match home_override {
            Some(home) => expand(home),
            None => default_home()?,
        };
        let explicit_config = path.is_some();
        let config_path = match path {
            Some(path) => expand(path.to_path_buf()),
            None => home.join("vps.toml"),
        };
        // Only the conventional path participates in first-run setup: a misspelled or
        // unavailable explicit path must fail rather than silently creating configuration.
        if !explicit_config && !config_path.exists() {
            initialize(&home, &config_path)?;
        }
        let mut c: Config = match fs::read_to_string(&config_path) {
            Ok(text) => toml::from_str(&text)?,
            Err(error) => return Err(error.into()),
        };
        if c.version != 1 {
            return Err(Error::Cli(format!(
                "unsupported configuration version {}",
                c.version
            )));
        };
        apply_environment(&mut c);
        c.ssh.private_key = c.ssh.private_key.map(expand);
        Ok((c, home.join("state/instances")))
    }
}
/// Applies supported environment overrides to an already populated configuration.
fn apply_environment(c: &mut Config) {
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
}
/// Interactively gathers first-run settings and writes a private, non-secret configuration file.
fn initialize(home: &Path, path: &Path) -> Result<()> {
    if !io::stdin().is_terminal() {
        return Err(Error::Cli(format!(
            "configuration not found at {}; run vps in an interactive terminal to initialize it",
            path.display()
        )));
    }

    console::heading("First-run setup", home.display().to_string());
    console::action("Configuration", "Answer the following questions");
    eprintln!("\n  Press Enter to accept a value shown in brackets.\n");

    let mut seed = Config::default();
    seed.ssh.private_key = Some(PathBuf::from("~/.ssh/id_digitalocean"));
    seed.git.author_name = git_config("user.name").unwrap_or(seed.git.author_name);
    seed.git.author_email = git_config("user.email").unwrap_or(seed.git.author_email);
    apply_environment(&mut seed);
    if seed.backends.digitalocean.ssh_key.is_none() {
        let keys = doctl_ssh_keys();
        if let [key] = keys.as_slice() {
            seed.backends.digitalocean.ssh_key = Some(key.id.clone());
        } else if !keys.is_empty() {
            eprintln!("  Available DigitalOcean SSH keys:");
            for key in keys {
                eprintln!("    {}  {}  {}", key.id, key.fingerprint, key.name);
            }
            eprintln!();
        }
    }

    let mut input = io::stdin().lock();
    let mut output = io::stderr().lock();
    let config = prompt_config(&mut input, &mut output, seed)?;
    // Persist only non-secret settings; cloud and repository credentials stay in their
    // environment and private lifecycle stores respectively.
    secure_write(path, format!(
        "{}\n\n# Set DIGITALOCEAN_TOKEN in the environment. Never put cloud or GitHub tokens\n# in this file.\n",
        toml::to_string_pretty(&config)?.trim_end()
    ).as_bytes())?;
    console::success("Configuration", format!("Saved {}", path.display()));
    Ok(())
}
/// Prompts for every required worker and DigitalOcean setting using values from `c` as defaults.
fn prompt_config<R: BufRead, W: Write>(
    reader: &mut R,
    writer: &mut W,
    mut c: Config,
) -> Result<Config> {
    let private_key = c
        .ssh
        .private_key
        .as_ref()
        .map(|path| path.to_string_lossy().into_owned());
    c.ssh.private_key = Some(PathBuf::from(ask(
        reader,
        writer,
        "SSH private key path",
        private_key.as_deref(),
        true,
    )?));
    c.git.author_name = ask(
        reader,
        writer,
        "Git author name",
        Some(&c.git.author_name),
        true,
    )?;
    c.git.author_email = ask(
        reader,
        writer,
        "Git author email",
        Some(&c.git.author_email),
        true,
    )?;
    let d = &mut c.backends.digitalocean;
    d.ssh_key = Some(ask(
        reader,
        writer,
        "DigitalOcean SSH key ID or fingerprint",
        d.ssh_key.as_deref(),
        true,
    )?);
    d.region = ask(reader, writer, "DigitalOcean region", Some(&d.region), true)?;
    d.size = ask(reader, writer, "DigitalOcean size", Some(&d.size), true)?;
    d.image = ask(reader, writer, "DigitalOcean image", Some(&d.image), true)?;
    let tags = d.tags.join(",");
    d.tags = ask(reader, writer, "DigitalOcean tags", Some(&tags), true)?
        .split(',')
        .map(str::trim)
        .filter(|tag| !tag.is_empty())
        .map(str::to_owned)
        .collect();
    d.name_prefix = ask(
        reader,
        writer,
        "Droplet name prefix",
        Some(&d.name_prefix),
        true,
    )?;
    Ok(c)
}
/// Reads one trimmed answer, accepting a nonempty default and retrying empty required values.
fn ask<R: BufRead, W: Write>(
    reader: &mut R,
    writer: &mut W,
    label: &str,
    default: Option<&str>,
    required: bool,
) -> Result<String> {
    loop {
        write!(writer, "  {label}")?;
        if let Some(default) = default.filter(|value| !value.is_empty()) {
            write!(writer, " [{default}]")?;
        }
        write!(writer, ": ")?;
        writer.flush()?;

        let mut answer = String::new();
        if reader.read_line(&mut answer)? == 0 {
            return Err(Error::Cli("configuration setup was interrupted".into()));
        }
        let answer = answer.trim();
        if !answer.is_empty() {
            return Ok(answer.to_owned());
        }
        if let Some(default) = default.filter(|value| !value.is_empty()) {
            return Ok(default.to_owned());
        }
        if !required {
            return Ok(String::new());
        }
        writeln!(writer, "    A value is required.")?;
    }
}
/// Reads a nonempty value from the user's Git configuration, returning `None` on any failure.
fn git_config(key: &str) -> Option<String> {
    let output = Command::new("git")
        .args(["config", "--get", key])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let value = String::from_utf8(output.stdout).ok()?;
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_owned())
}
struct DoctlSshKey {
    id: String,
    fingerprint: String,
    name: String,
}
/// Lists DigitalOcean SSH keys through `doctl`, returning no suggestions when the command fails.
fn doctl_ssh_keys() -> Vec<DoctlSshKey> {
    let Ok(output) = Command::new("doctl")
        .args([
            "compute",
            "ssh-key",
            "list",
            "--format",
            "ID,FingerPrint,Name",
            "--no-header",
        ])
        .output()
    else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    String::from_utf8(output.stdout)
        .map(|stdout| parse_doctl_ssh_keys(&stdout))
        .unwrap_or_default()
}
/// Parses `doctl`'s whitespace-separated ID, fingerprint, and possibly spaced name columns.
fn parse_doctl_ssh_keys(output: &str) -> Vec<DoctlSshKey> {
    output
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            Some(DoctlSshKey {
                id: fields.next()?.to_owned(),
                fingerprint: fields.next()?.to_owned(),
                name: fields.collect::<Vec<_>>().join(" "),
            })
        })
        .collect()
}
/// Writes `contents` without exposing the file to other users on supported platforms.
fn secure_write(path: &Path, contents: &[u8]) -> Result<()> {
    let directory = path
        .parent()
        .ok_or_else(|| Error::Cli("configuration path has no parent".into()))?;
    fs::create_dir_all(directory)?;
    secure_dir(directory)?;
    let temporary = directory.join(format!(".vps.toml.tmp-{}", Uuid::new_v4()));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temporary)?;
    file.write_all(contents)?;
    file.sync_all()?;
    drop(file);
    if path.exists() {
        fs::remove_file(temporary)?;
        return Err(Error::Cli(format!(
            "configuration already exists at {}",
            path.display()
        )));
    }
    fs::rename(temporary, path)?;
    sync_dir(directory)?;
    Ok(())
}
/// Sets the configuration directory to owner-only access on Unix.
#[cfg(unix)]
fn secure_dir(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}
/// Leaves configuration-directory ACL management to the platform on non-Unix systems.
#[cfg(not(unix))]
fn secure_dir(_: &Path) -> Result<()> {
    Ok(())
}
/// Flushes the newly renamed configuration entry to durable storage on Unix.
#[cfg(unix)]
fn sync_dir(path: &Path) -> Result<()> {
    File::open(path)?.sync_all()?;
    Ok(())
}
/// Performs no directory sync where `std::fs::File` cannot open directories.
#[cfg(not(unix))]
fn sync_dir(_: &Path) -> Result<()> {
    Ok(())
}
/// Returns the default home used when no explicit value is configured.
fn default_home() -> Result<PathBuf> {
    user_home()
        .map(|home| home.join(".vps"))
        .ok_or_else(|| Error::Cli("cannot determine the user home directory".into()))
}
/// Returns the platform user home directory when it can be determined.
fn user_home() -> Option<PathBuf> {
    BaseDirs::new().map(|dirs| dirs.home_dir().to_path_buf())
}
/// Expands a leading `~` or `~/` against the platform user home, leaving other paths unchanged.
fn expand(p: PathBuf) -> PathBuf {
    let s = p.to_string_lossy();
    if (s == "~" || s.starts_with("~/"))
        && let Some(home) = user_home()
    {
        return home.join(s.trim_start_matches("~/"));
    }
    p
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Default state and configuration are rooted beneath the user's `.vps` directory.
    #[test]
    fn defaults_are_beneath_vps_home() {
        let root = default_home().unwrap();

        assert_eq!(root, user_home().unwrap().join(".vps"));
    }

    /// A custom VPS home controls both the default config path and returned state path.
    #[test]
    fn custom_home_owns_config_and_state() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("custom-home");
        let config = home.join("vps.toml");
        fs::create_dir_all(&home).unwrap();
        fs::write(
            &config,
            "version = 1\n[git]\nauthor_name = \"Custom home\"\n",
        )
        .unwrap();

        let (config, state) = Config::load(None, Some(home.clone())).unwrap();

        assert_eq!(config.git.author_name, "Custom home");
        assert_eq!(state, home.join("state/instances"));
    }

    /// First-run prompts retain defaults and populate every required setting.
    #[test]
    fn setup_prompts_populate_complete_config() {
        let mut seed = Config::default();
        seed.ssh.private_key = Some("~/.ssh/id_test".into());
        seed.git.author_name = "Existing Name".into();
        seed.git.author_email = "existing@example.com".into();
        let answers = b"\n\n\n123456\n\n\n\n\n\n";
        let mut output = Vec::new();

        let config = prompt_config(&mut &answers[..], &mut output, seed).unwrap();

        assert_eq!(
            config.ssh.private_key.unwrap(),
            PathBuf::from("~/.ssh/id_test")
        );
        assert_eq!(config.git.author_name, "Existing Name");
        assert_eq!(config.git.author_email, "existing@example.com");
        assert_eq!(
            config.backends.digitalocean.ssh_key.as_deref(),
            Some("123456")
        );
        assert_eq!(config.backends.digitalocean.region, "nyc3");
        assert_eq!(config.backends.digitalocean.size, "s-4vcpu-8gb");
        assert_eq!(config.backends.digitalocean.image, "ubuntu-24-04-x64");
        assert_eq!(config.backends.digitalocean.tags, vec!["codex-agent"]);
        assert_eq!(config.backends.digitalocean.name_prefix, "codex-agent");
    }

    /// `doctl` key parsing preserves names containing spaces and ignores malformed rows.
    #[test]
    fn parses_doctl_key_choices_and_names() {
        let keys = parse_doctl_ssh_keys("123456 aa:bb personal key\n789012 cc:dd automation\n");

        assert_eq!(keys.len(), 2);
        assert_eq!(keys[0].id, "123456");
        assert_eq!(keys[0].fingerprint, "aa:bb");
        assert_eq!(keys[0].name, "personal key");
        assert_eq!(keys[1].id, "789012");
    }

    /// First-run configuration is persisted with owner-only permissions on Unix.
    #[cfg(unix)]
    #[test]
    fn setup_writes_private_config() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let path = home.join("vps.toml");

        secure_write(&path, b"version = 1\n").unwrap();

        assert_eq!(
            fs::metadata(&home).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}
