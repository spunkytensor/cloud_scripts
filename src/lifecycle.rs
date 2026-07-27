use crate::{
    backend::{Backend, Mutation},
    config::Config,
    console,
    error::{Error, Result},
    model::*,
    ssh,
    state::Store,
};
use std::{
    collections::BTreeMap,
    fs,
    io::{self, IsTerminal},
    path::Path,
    time::Duration,
};
use uuid::Uuid;

pub struct ControlPlane<'a> {
    pub store: &'a Store,
    pub config: &'a Config,
    pub backend: &'a dyn Backend,
}
fn shq(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}
fn prompt_github_token(repository: &str, server_id: &str, replacement: bool) -> Result<String> {
    console::action("GitHub", "Authorization required");
    eprintln!(
        "\n    {}\n\n    Create a fine-grained PAT that:\n      • targets only {repository}\n      • expires in two days\n      • grants Contents and Pull requests: read and write\n      • grants Actions and Commit statuses: read\n\n    Paste it below; input will not be echoed.\n",
        crate::github::creation_url(repository, server_id)
    );
    if !io::stdin().is_terminal() {
        return Err(Error::Cli(
            "GitHub token provisioning requires an interactive terminal".into(),
        ));
    }
    let label = if replacement {
        "Replacement fine-grained GitHub PAT: "
    } else {
        "VPS-specific fine-grained GitHub PAT: "
    };
    Ok(rpassword::prompt_password(label)?.trim().to_owned())
}
fn chatgpt_logged_in(server: &Server, key: &Path, known: &Path) -> Result<bool> {
    Ok(ssh::capture(
        server,
        key,
        known,
        "/usr/local/bin/codex login status >/dev/null 2>&1 && echo yes || echo no",
    )? == "yes")
}
pub fn remote_usable(v: &serde_json::Value) -> bool {
    v.get("status")
        .and_then(|x| x.as_str())
        .is_some_and(|s| matches!(s, "connected" | "connecting"))
        || v.get("state")
            .and_then(|x| x.as_str())
            .is_some_and(|s| matches!(s, "connected" | "connecting"))
}
pub fn pairing_code(v: &serde_json::Value) -> Option<&str> {
    v.get("manualPairingCode")
        .or_else(|| v.get("code"))
        .or_else(|| v.get("pairing_code"))
        .and_then(|x| x.as_str())
        .filter(|s| !s.is_empty())
}

pub fn validate_snapshot(
    found: &Snapshot,
    expected_id: &str,
    expected_name: &str,
    expected_source: &str,
    source_region: &str,
    source_disk_gb: u64,
) -> Result<()> {
    if found.id != expected_id
        || found.name != expected_name
        || found.source_id != expected_source
        || !found.regions.iter().any(|region| region == source_region)
        || found.min_disk_gb > source_disk_gb
    {
        return Err(Error::State(
            "snapshot identity or minimum disk is incompatible with source".into(),
        ));
    }
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
pub struct PauseMarker {
    pub operation_id: String,
    pub host_key: String,
    pub running_container_ids: Vec<String>,
    pub healthy_container_ids: Vec<String>,
}

pub fn parse_pause_marker(input: &str) -> Result<PauseMarker> {
    const KEYS: [&str; 4] = [
        "OPERATION_ID",
        "SSH_HOST_ED25519_PUBLIC_KEY",
        "RUNNING_CONTAINER_IDS",
        "HEALTHY_CONTAINER_IDS",
    ];
    let mut fields = BTreeMap::new();
    for line in input.lines() {
        let (key, _) = line
            .split_once('=')
            .ok_or_else(|| Error::Remote("pause marker contains a malformed field".into()))?;
        if !KEYS.contains(&key) || fields.contains_key(key) {
            return Err(Error::Remote(
                "pause marker contains unknown or duplicate fields".into(),
            ));
        }
        let parsed = crate::state::legacy::parse(&format!("{line}\n"))?;
        fields.insert(key.to_owned(), parsed[key].clone());
    }
    if fields.len() != KEYS.len() {
        return Err(Error::Remote(
            "pause marker is missing required fields".into(),
        ));
    }
    let ids = |key: &str| -> Result<Vec<String>> {
        fields[key]
            .split_whitespace()
            .map(|id| {
                if (12..=64).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_hexdigit()) {
                    Ok(id.to_owned())
                } else {
                    Err(Error::Remote(format!(
                        "pause marker has an invalid container ID in {key}"
                    )))
                }
            })
            .collect()
    };
    Ok(PauseMarker {
        operation_id: fields["OPERATION_ID"].clone(),
        host_key: fields["SSH_HOST_ED25519_PUBLIC_KEY"].clone(),
        running_container_ids: ids("RUNNING_CONTAINER_IDS")?,
        healthy_container_ids: ids("HEALTHY_CONTAINER_IDS")?,
    })
}

#[derive(Debug, Clone, Copy, Default)]
pub struct PauseOptions {
    pub confirm_missing_server: bool,
    pub confirm_request_not_accepted: bool,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct ResumeOptions {
    pub confirm_missing_snapshot: bool,
    pub confirm_request_not_accepted: bool,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct DestroyOptions {
    pub confirm_missing_server: bool,
    pub confirm_missing_snapshot: bool,
    pub confirm_request_not_accepted: bool,
    pub forget_unresolved_allocation: bool,
    pub forget_unrevoked_token: bool,
}
impl ControlPlane<'_> {
    async fn allocate_pending(&self, i: &mut Instance) -> Result<Server> {
        let (recipe, correlation, request_intent) = match &i.lifecycle {
            Lifecycle::AllocationPending {
                recipe,
                correlation,
                request_intent,
            } => (recipe.clone(), correlation.clone(), *request_intent),
            _ => unreachable!(),
        };
        console::pending("Provider", "Checking for an existing correlated Droplet");
        let matches = self.backend.find_servers(&correlation).await?;
        match matches.len() {
            1 => {
                console::success("Provider", format!("Resuming Droplet {}", matches[0].id));
                return Ok(matches[0].clone());
            }
            n if n > 1 => {
                return Err(Error::Uncertain(format!(
                    "persisted allocation correlation has {n} matches; refusing another create"
                )));
            }
            _ => {}
        }
        if request_intent {
            return Err(Error::Uncertain(
                "persisted allocation request has no correlation match; refusing another create"
                    .into(),
            ));
        }

        let public = public_key(self.config.ssh.private_key.as_deref())?;
        let cloud =
            include_str!("../cloud-init.yaml").replace("__AGENT_SSH_AUTHORIZED_KEY__", &public);
        if let Lifecycle::AllocationPending { request_intent, .. } = &mut i.lifecycle {
            *request_intent = true;
        }
        self.store.save(i)?;
        console::pending(
            "Provider",
            format!(
                "Creating Droplet {} ({}, {}, {})",
                recipe.name, recipe.region, recipe.size, recipe.image
            ),
        );
        match self.backend.create_server(&recipe, Some(&cloud)).await? {
            Mutation::Confirmed(server) => {
                console::success("Provider", format!("Created Droplet {}", server.id));
                Ok(server)
            }
            Mutation::Uncertain { diagnostic } => {
                let matches = self.backend.find_servers(&correlation).await?;
                if matches.len() == 1 {
                    Ok(matches[0].clone())
                } else {
                    Err(Error::Uncertain(format!(
                        "{diagnostic}; correlation {correlation} has {} matches",
                        matches.len()
                    )))
                }
            }
            Mutation::Rejected { diagnostic } => {
                if let Lifecycle::AllocationPending { request_intent, .. } = &mut i.lifecycle {
                    *request_intent = false;
                }
                self.store.save(i)?;
                Err(Error::Backend(diagnostic))
            }
        }
    }

    async fn install_github_token(
        &self,
        i: &Instance,
        server: &Server,
        known: &Path,
    ) -> Result<String> {
        let dir = self.store.dir(&i.instance_id);
        let primary = crate::github::retained_token(&dir)?;
        let mut replacement = crate::github::replacement(&dir)?;
        if let Some(journal) = &replacement
            && primary
                .as_deref()
                .is_none_or(|token| token != journal.old && token != journal.new)
        {
            return Err(Error::State(
                "replacement journal does not match the retained primary token".into(),
            ));
        }
        let (token, actor) = if let Some(journal) = &replacement {
            console::pending("GitHub", "Validating the pending replacement credential");
            let actor = crate::github::validate(&journal.new, &i.repository).await?;
            (journal.new.clone(), actor)
        } else if let Some(old) = &primary {
            console::pending("GitHub", "Validating the retained repository credential");
            match crate::github::validate(old, &i.repository).await {
                Ok(actor) => (old.clone(), actor),
                Err(Error::Cli(message)) if message == "GitHub PAT does not identify a user" => {
                    console::action("GitHub", "Retained credential needs replacement");
                    let new = prompt_github_token(&i.repository, &server.id, true)?;
                    console::pending("GitHub", "Validating the replacement credential");
                    let actor = crate::github::validate(&new, &i.repository).await?;
                    let actor_file = dir.join("github-user");
                    if actor_file.exists() && fs::read_to_string(&actor_file)?.trim() != actor {
                        return Err(Error::Cli(
                            "replacement PAT identifies a different GitHub user".into(),
                        ));
                    }
                    self.store.save_secret(
                        &i.instance_id,
                        "github-token.replacement",
                        &crate::github::replacement_bytes(old, &new)?,
                    )?;
                    replacement = Some(crate::github::Replacement {
                        old: old.clone(),
                        new: new.clone(),
                    });
                    (new, actor)
                }
                Err(error) => return Err(error),
            }
        } else {
            let token = prompt_github_token(&i.repository, &server.id, false)?;
            console::pending("GitHub", "Validating the repository credential");
            let actor = crate::github::validate(&token, &i.repository).await?;
            // Establish local ownership before the first possible remote mutation.
            self.store
                .save_secret(&i.instance_id, "github-token", token.as_bytes())?;
            (token, actor)
        };

        let actor_file = dir.join("github-user");
        if replacement.is_some()
            && actor_file.exists()
            && fs::read_to_string(&actor_file)?.trim() != actor
        {
            return Err(Error::Cli(
                "replacement PAT identifies a different GitHub user".into(),
            ));
        }

        // Refuse to overwrite an unowned remote credential. Send candidates over stdin so
        // neither token appears in argv, diagnostics, or command output.
        console::pending(
            "GitHub",
            "Installing the repository credential on the worker",
        );
        let old = replacement
            .as_ref()
            .map_or(token.as_str(), |r| r.old.as_str());
        let classify = ssh::stdin(
            server,
            key(self.config)?,
            known,
            "set -euo pipefail; d=/home/agent/.config/vps-codex; f=$d/github-token; IFS= read -r old; IFS= read -r new; if [[ ! -e $f ]]; then echo absent; elif cmp -s $f <(printf %s \"$old\") || cmp -s $f <(printf %s \"$new\"); then echo retained; else exit 42; fi",
            format!("{old}\n{token}\n").as_bytes(),
            false,
        );
        if classify.is_err() {
            return Err(Error::State(
                "worker has a foreign, untracked GitHub token; refusing overwrite".into(),
            ));
        }
        // Initial/replacement local evidence is durable before this remote mutation.
        ssh::stdin(
            server,
            key(self.config)?,
            known,
            "set -euo pipefail; d=/home/agent/.config/vps-codex; install -d -m 0700 \"$d\"; tmp=$(mktemp \"$d/.github-token.tmp.XXXXXX\"); trap 'rm -f \"$tmp\"' EXIT; chmod 0600 \"$tmp\"; cat >\"$tmp\"; mv -f \"$tmp\" \"$d/github-token\"; trap - EXIT",
            token.as_bytes(),
            false,
        )?;
        self.store
            .save_secret(&i.instance_id, "github-token", token.as_bytes())?;
        self.store
            .save_secret(&i.instance_id, "github-user", actor.as_bytes())?;
        if let Some(journal) = replacement {
            // Keep old/new evidence on any revocation failure; rerun repeats this exact sequence.
            crate::github::revoke(&journal.old).await?;
            self.store
                .remove_secret(&i.instance_id, "github-token.replacement")?;
        }
        Ok(actor)
    }

    pub async fn create(&self, id: String, repository: String, branch: String) -> Result<Instance> {
        validate_instance_id(&id)?;
        validate_repository(&repository)?;
        validate_branch(&branch)?;
        console::heading("Create", format!("{id}  ·  {repository}@{branch}"));
        let _lock = self.store.lock(&id)?;
        if self.store.dir(&id).join("instance.json").exists()
            || self.store.dir(&id).join("current.env.setup").exists()
        {
            let mut i = self.store.load_or_adopt_locked(&id)?;
            if i.repository != repository || i.base_branch != branch {
                return Err(Error::State(
                    "existing instance source does not match requested source".into(),
                ));
            }
            if self.store.transition(&id)?.is_some() {
                return Err(Error::State("instance has an unresolved pause/resume/destroy transition; rerun that command".into()));
            }
            if matches!(i.lifecycle, Lifecycle::SourceReserved { .. }) {
                let d = &self.config.backends.digitalocean;
                let correlation = format!("vps-{}", Uuid::new_v4());
                let mut tags = d.tags.clone();
                tags.push(correlation.clone());
                i.lifecycle = Lifecycle::AllocationPending {
                    recipe: CreateRecipe {
                        name: format!("{}-{id}", d.name_prefix),
                        region: d.region.clone(),
                        size: d.size.clone(),
                        image: d.image.clone(),
                        tags,
                        ssh_key: d.ssh_key.clone().ok_or_else(|| {
                            Error::Cli("DigitalOcean SSH key is not configured".into())
                        })?,
                    },
                    correlation,
                    request_intent: false,
                };
                self.store.save(&i)?;
            }
            if matches!(i.lifecycle, Lifecycle::AllocationPending { .. }) {
                let server = self.allocate_pending(&mut i).await?;
                if let Lifecycle::AllocationPending { correlation, .. } = &i.lifecycle
                    && !server.tags.contains(correlation)
                {
                    return Err(Error::State(
                        "created server lacks persisted correlation tag".into(),
                    ));
                }
                let server = self.provision(&i, server).await?;
                i.lifecycle = Lifecycle::Active {
                    server,
                    snapshot: None,
                };
                self.store.save(&i)?;
            }
            return Ok(i);
        }
        let d = &self.config.backends.digitalocean;
        let ssh_key = d
            .ssh_key
            .clone()
            .ok_or_else(|| Error::Cli("DigitalOcean SSH key is not configured".into()))?;
        let correlation = format!("vps-{}", Uuid::new_v4());
        let mut tags = d.tags.clone();
        tags.push(correlation.clone());
        let recipe = CreateRecipe {
            name: format!("{}-{id}", d.name_prefix),
            region: d.region.clone(),
            size: d.size.clone(),
            image: d.image.clone(),
            tags,
            ssh_key,
        };
        let mut i = Instance {
            schema_version: 1,
            instance_id: id.clone(),
            backend: "digitalocean".into(),
            created_at: now(),
            repository,
            base_branch: branch,
            work_branch: format!("codex/{id}"),
            lifecycle: Lifecycle::AllocationPending {
                recipe: recipe.clone(),
                correlation: correlation.clone(),
                request_intent: false,
            },
            provider: serde_json::json!({}),
        };
        self.store.save(&i)?;
        let server = self.allocate_pending(&mut i).await?;
        if !server.tags.contains(&correlation) {
            return Err(Error::State(
                "created server lacks persisted correlation tag".into(),
            ));
        }
        let server = self.provision(&i, server).await?;
        i.lifecycle = Lifecycle::Active {
            server,
            snapshot: None,
        };
        self.store.save(&i)?;
        Ok(i)
    }

    async fn provision(&self, i: &Instance, mut server: Server) -> Result<Server> {
        // Always poll the persisted provider ID, never a name or a newly allocated server.
        console::pending(
            "Provider",
            format!("Waiting for Droplet {} public networking", server.id),
        );
        for _ in 0..60 {
            server = self.backend.get_server(&server.id).await?.ok_or_else(|| {
                Error::Uncertain("allocated server is not visible by its exact ID".into())
            })?;
            if server.endpoint.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
        if server.endpoint.is_none() {
            return Err(Error::Uncertain(
                "allocated server has no public endpoint".into(),
            ));
        }
        console::success(
            "Provider",
            format!(
                "Droplet {} is reachable at {}",
                server.id,
                server.endpoint.as_deref().unwrap()
            ),
        );
        let key = key(self.config)?;
        let known = self.store.dir(&i.instance_id).join("known_hosts");
        let ready = b"set -euo pipefail\ncloud-init status --wait >/dev/null\ntest -f /opt/codex-worker-ready\ndocker info >/dev/null\n";
        let mut last = None;
        console::pending(
            "Worker",
            "Waiting for SSH, cloud-init, and Docker (this can take several minutes)",
        );
        for _ in 0..60 {
            match ssh::stdin(&server, key, &known, "bash -s", ready, true) {
                Ok(_) => {
                    last = None;
                    break;
                }
                Err(e) => {
                    last = Some(e);
                    tokio::time::sleep(Duration::from_secs(5)).await;
                }
            }
        }
        if let Some(e) = last {
            return Err(e);
        }
        console::success("Worker", "Bootstrap complete");

        let dir = self.store.dir(&i.instance_id);
        let actor = self.install_github_token(i, &server, &known).await?;
        console::pending(
            "Workspace",
            format!("Preparing {} on {}", i.repository, i.work_branch),
        );
        let setup = format!(
            r#"set -euo pipefail
repo={repo}; base={base}; work={work}; name={name}; email={email}
origin="https://github.com/${{repo}}.git"; checkout=/home/agent/projects/${{repo#*/}}
export GH_TOKEN="$(cat /home/agent/.config/vps-codex/github-token)"
gh auth setup-git --hostname github.com --force
if [[ ! -e "$checkout" ]]; then git clone --branch "$base" --single-branch "$origin" "$checkout"; fi
[[ -d "$checkout/.git" ]]; cd "$checkout"
[[ "$(git remote get-url origin)" == "$origin" ]]
git ls-remote --exit-code origin "refs/heads/$base" >/dev/null
git config user.name "$name"; git config user.email "$email"
if ! git show-ref --verify --quiet "refs/heads/$work"; then
  [[ -z "$(git status --porcelain)" ]]; git switch -c "$work"
else git switch "$work"; fi
[[ "$(git symbolic-ref --short HEAD)" == "$work" ]]
"#,
            repo = shq(&i.repository),
            base = shq(&i.base_branch),
            work = shq(&i.work_branch),
            name = shq(&self.config.git.author_name),
            email = shq(&self.config.git.author_email)
        );
        ssh::stdin(&server, key, &known, "bash -s", setup.as_bytes(), false)?;
        console::success("Workspace", "Repository checkout ready");

        let marker = dir.join("chatgpt-login.json");
        console::pending("ChatGPT", "Checking worker login");
        let logged_in = chatgpt_logged_in(&server, key, &known)?;
        if logged_in && !marker.exists() {
            return Err(Error::State(
                "worker has a foreign, untracked ChatGPT login; log it out before retrying".into(),
            ));
        }
        if !logged_in {
            // Persist ownership intent before the interactive operation so a crash after a
            // successful login remains safely resumable.
            self.store.save_secret(
                &i.instance_id,
                "chatgpt-login.json",
                br#"{"schema_version":1,"managed":true}"#,
            )?;
            console::action("ChatGPT", "Complete the device login shown below");
            ssh::run(
                &server,
                key,
                &known,
                &["/usr/local/bin/codex", "login", "--device-auth"],
                true,
            )?;
            if !chatgpt_logged_in(&server, key, &known)? {
                return Err(Error::Remote("ChatGPT login verification failed".into()));
            }
        }
        console::pending("Remote control", "Starting and requesting a pairing code");
        let start: serde_json::Value = serde_json::from_str(&ssh::capture(
            &server,
            key,
            &known,
            "/usr/local/bin/codex remote-control --json start",
        )?)?;
        if !remote_usable(&start) {
            return Err(Error::Remote(
                "remote control did not report connected or connecting".into(),
            ));
        }
        let pair: serde_json::Value = serde_json::from_str(&ssh::capture(
            &server,
            key,
            &known,
            "/usr/local/bin/codex remote-control --json pair",
        )?)?;
        if pairing_code(&pair).is_none() {
            return Err(Error::Remote(
                "remote control returned no pairing code".into(),
            ));
        }
        self.store.save_secret(
            &i.instance_id,
            "remote-control.json",
            br#"{"schema_version":1,"enrolled":true}"#,
        )?;
        console::success("GitHub", format!("Authenticated as {actor}"));
        console::action(
            "Pairing code",
            format!(
                "Enter {} in the controlling client",
                pairing_code(&pair).unwrap()
            ),
        );
        Ok(server)
    }
    pub async fn pause(&self, id: &str, options: PauseOptions) -> Result<Instance> {
        console::heading("Pause", id);
        let _lock = self.store.lock_existing(id)?;
        let i = self.store.load_or_adopt_locked(id)?;
        let existing = self.store.transition(id)?;
        let server = existing
            .as_ref()
            .filter(|t| t.kind == TransitionKind::Pause)
            .and_then(|t| t.source.clone())
            .or_else(|| match &i.lifecycle {
                Lifecycle::Active {
                    server,
                    snapshot: None,
                } => Some(server.clone()),
                _ => None,
            })
            .ok_or_else(|| {
                Error::State("pause requires an active instance with no retained snapshot".into())
            })?;
        if let Some(t) = &existing
            && t.kind != TransitionKind::Pause
        {
            return Err(Error::State(
                "a non-pause transition is already in progress".into(),
            ));
        }
        if existing
            .as_ref()
            .is_some_and(|t| t.phase == Phase::PausingDeletePending)
        {
            return self
                .finish_pause_delete(id, i, existing.unwrap(), options)
                .await;
        }
        console::pending("Provider", "Verifying the active Droplet");
        let current = self.backend.get_server(&server.id).await?.ok_or_else(|| {
            if options.confirm_missing_server {
                Error::State("confirmed missing active server cannot be snapshotted; destroy the retained state".into())
            } else {
                Error::Uncertain("active server is missing; rerun with --confirm-missing-server, then destroy the retained state".into())
            }
        })?;
        if current.id != server.id || !current.volume_ids.is_empty() {
            return Err(Error::State(
                "provider server identity changed or has attached volumes".into(),
            ));
        }
        console::success("Provider", "Active Droplet verified");
        let op = Uuid::new_v4().to_string();
        let mut t = existing.unwrap_or(Transition {
            schema_version: 1,
            kind: TransitionKind::Pause,
            phase: Phase::PausingQuiescing,
            operation_id: op.clone(),
            started_at: now(),
            checkpoints: Default::default(),
            source: Some(server.clone()),
            snapshot: None,
            target_recipe: None,
            target: None,
            correlation: None,
        });
        if t.kind != TransitionKind::Pause {
            return Err(Error::State(
                "a non-pause transition is already in progress".into(),
            ));
        }
        t.validate(&i)?;
        self.store.save_transition(id, &t)?;
        let key = key(self.config)?;
        let known = self.store.dir(id).join("known_hosts");
        let name = format!("vps-{id}-{}", t.operation_id);
        let mut host_key = t.checkpoints.captured_host_key.clone().unwrap_or_default();
        if t.phase == Phase::PausingQuiescing {
            console::pending("Worker", "Stopping remote control and running containers");
            let command = format!(
                r#"bash -s -- '{}' <<'VPS_PAUSE'
set -euo pipefail
op="$1"; cfg=/etc/cloud/cloud.cfg.d/99-codex-snapshot-host-key.cfg; marker=/home/agent/.config/vps-codex/pause.env
[[ "$(stat -c '%U:%G:%a' "$cfg")" == root:root:644 ]]; grep -Fxq 'ssh_deletekeys: false' "$cfg"
install -d -m 0700 "$(dirname "$marker")"
if [[ -f "$marker" ]]; then source "$marker"; [[ "$OPERATION_ID" == "$op" ]]; printf '%s\n' "$SSH_HOST_ED25519_PUBLIC_KEY"; exit; fi
/usr/local/bin/codex remote-control --json stop >/dev/null 2>&1 || true
running="$(docker ps -q | tr '\n' ' ')"; healthy="$(docker ps --filter health=healthy -q | tr '\n' ' ')"
[[ -z "${{running// /}}" ]] || docker stop --time 120 $running >/dev/null
for c in $running; do [[ "$(docker inspect -f '{{{{.State.Running}}}}' "$c")" == false ]]; done
sync; host="$(cat /etc/ssh/ssh_host_ed25519_key.pub)"; tmp="$(mktemp "$marker.tmp.XXXXXX")"; chmod 0600 "$tmp"
printf 'OPERATION_ID=%q\nSSH_HOST_ED25519_PUBLIC_KEY=%q\nRUNNING_CONTAINER_IDS=%q\nHEALTHY_CONTAINER_IDS=%q\n' "$op" "$host" "$running" "$healthy" >"$tmp"; mv "$tmp" "$marker"; printf '%s\n' "$host"
VPS_PAUSE"#,
                t.operation_id
            );
            host_key = ssh::capture(&server, key, &known, &command)?;
            if !host_key.starts_with("ssh-ed25519 ") {
                return Err(Error::Remote("invalid Ed25519 host key".into()));
            }
            let trusted = fs::read_to_string(&known)?;
            let identity = |v: &str| {
                let fields: Vec<_> = v.split_whitespace().collect();
                fields
                    .windows(2)
                    .find(|w| w[0] == "ssh-ed25519")
                    .map(|w| format!("{} {}", w[0], w[1]))
            };
            if identity(&trusted) != identity(&host_key) {
                return Err(Error::Remote(
                    "captured host key does not match trusted known_hosts".into(),
                ));
            }
            t.checkpoints.captured_host_key = Some(host_key.clone());
            t.checkpoints.quiescence_verified = true;
            t.phase = Phase::PausingShutdown;
            self.store.save_transition(id, &t)?;
            console::success("Worker", "Workloads stopped and disk synchronized");
        }
        if t.phase == Phase::PausingShutdown {
            console::pending("Provider", "Shutting down the Droplet");
            let a = self
                .continue_action(
                    &mut t,
                    id,
                    &server.id,
                    "shutdown",
                    None,
                    true,
                    options.confirm_request_not_accepted,
                )
                .await?;
            self.backend.wait_action(&a).await?;
            let refreshed = self
                .backend
                .get_server(&server.id)
                .await?
                .ok_or_else(|| Error::Uncertain("server disappeared during shutdown".into()))?;
            if refreshed.status != "off" {
                let power = self
                    .continue_power_off(
                        &mut t,
                        id,
                        &server.id,
                        options.confirm_request_not_accepted,
                    )
                    .await?;
                self.backend.wait_action(&power).await?;
                let powered = self.backend.get_server(&server.id).await?.ok_or_else(|| {
                    Error::Uncertain("server disappeared during hard power-off".into())
                })?;
                if powered.status != "off" {
                    return Err(Error::Uncertain(
                        "hard power-off completed but server is not off".into(),
                    ));
                }
            }
            t.phase = Phase::PausingSnapshot;
            self.store.save_transition(id, &t)?;
            console::success("Provider", "Droplet is powered off");
        }
        if t.phase == Phase::PausingSnapshot {
            console::pending("Snapshot", "Creating the recovery snapshot");
            let a = self
                .continue_action(
                    &mut t,
                    id,
                    &server.id,
                    "snapshot",
                    Some(&name),
                    false,
                    options.confirm_request_not_accepted,
                )
                .await?;
            self.backend.wait_action(&a).await?;
        }
        let mut matches: Vec<_> = self
            .backend
            .snapshots()
            .await?
            .into_iter()
            .filter(|s| {
                s.name == name
                    && s.source_id == server.id
                    && s.regions.iter().any(|region| region == &server.region)
            })
            .collect();
        if matches.len() != 1 {
            return Err(Error::Uncertain(format!(
                "snapshot discovery found {} exact matches",
                matches.len()
            )));
        }
        let discovered = matches.remove(0);
        let mut snap = self
            .backend
            .get_snapshot(&discovered.id)
            .await?
            .ok_or_else(|| {
                Error::Uncertain("discovered snapshot disappeared before verification".into())
            })?;
        validate_snapshot(
            &snap,
            &discovered.id,
            &name,
            &server.id,
            &server.region,
            server.disk_gb,
        )?;
        snap.region = server.region.clone();
        snap.host_key = host_key;
        snap.pause_operation_id = t.operation_id.clone();
        snap.source_recipe = Some(CreateRecipe {
            name: server.name.clone(),
            region: server.region.clone(),
            size: server.size.clone(),
            image: server.image.clone(),
            tags: server.tags.clone(),
            ssh_key: self
                .config
                .backends
                .digitalocean
                .ssh_key
                .clone()
                .ok_or_else(|| Error::Cli("SSH key missing".into()))?,
        });
        t.snapshot = Some(snap.clone());
        t.phase = Phase::PausingDeletePending;
        t.checkpoints.source_delete_intent = true;
        self.store.save_transition(id, &t)?;
        console::success("Snapshot", "Recovery snapshot verified");
        self.finish_pause_delete(id, i, t, options).await
    }

    async fn finish_pause_delete(
        &self,
        id: &str,
        mut i: Instance,
        mut t: Transition,
        options: PauseOptions,
    ) -> Result<Instance> {
        let source = t
            .source
            .clone()
            .ok_or_else(|| Error::State("pause source missing".into()))?;
        let snap = t
            .snapshot
            .clone()
            .ok_or_else(|| Error::State("pause snapshot missing".into()))?;
        console::pending("Provider", "Deleting the paused source Droplet");
        if self.backend.get_server(&source.id).await?.is_some() {
            confirmed(self.backend.delete_server(&source.id).await?)?;
        } else if !t.checkpoints.source_delete_confirmed && !options.confirm_missing_server {
            return Err(Error::Uncertain(
                "pause source is missing; --confirm-missing-server is required".into(),
            ));
        }
        t.checkpoints.source_delete_confirmed = true;
        self.store.save_transition(id, &t)?;
        fs::remove_file(self.store.dir(id).join("known_hosts")).ok();
        i.lifecycle = Lifecycle::Paused { snapshot: snap };
        self.store.save(&i)?;
        self.store.clear_transition(id)?;
        console::success(
            "Provider",
            "Source Droplet deleted; compute billing stopped",
        );
        Ok(i)
    }

    #[allow(clippy::too_many_arguments)]
    async fn continue_action(
        &self,
        t: &mut Transition,
        id: &str,
        resource: &str,
        kind: &str,
        name: Option<&str>,
        shutdown: bool,
        confirm_request_not_accepted: bool,
    ) -> Result<Action> {
        let action_id = if shutdown {
            t.checkpoints.shutdown_action.clone()
        } else {
            t.checkpoints.snapshot_action.clone()
        };
        if let Some(action_id) = action_id {
            return self.backend.get_action(&action_id).await?.ok_or_else(|| {
                Error::Uncertain(format!("persisted action {action_id} is missing"))
            });
        }
        let intent = if shutdown {
            t.checkpoints.shutdown_intent
        } else {
            t.checkpoints.snapshot_intent
        };
        if intent {
            let found = self
                .backend
                .find_actions(resource, kind, &t.started_at)
                .await?;
            if found.len() == 1 {
                if shutdown {
                    t.checkpoints.shutdown_action = Some(found[0].id.clone())
                } else {
                    t.checkpoints.snapshot_action = Some(found[0].id.clone())
                }
                self.store.save_transition(id, t)?;
                return Ok(found[0].clone());
            }
            if !found.is_empty() {
                return Err(Error::Uncertain(format!(
                    "{} matching {kind} actions found",
                    found.len()
                )));
            }
            if !confirm_request_not_accepted {
                console::action(
                    "Confirmation",
                    format!(
                        "Verify no {kind} action exists, then rerun with --confirm-request-not-accepted"
                    ),
                );
                return Err(Error::Uncertain(format!(
                    "no matching {kind} action found; explicit confirmation is required before resubmission"
                )));
            }
            if shutdown {
                t.checkpoints.shutdown_intent = false
            } else {
                t.checkpoints.snapshot_intent = false
            };
            self.store.save_transition(id, t)?;
            console::success(
                "Confirmation",
                format!("Prior {kind} request confirmed absent; resubmitting"),
            );
        }
        if shutdown {
            t.checkpoints.shutdown_intent = true
        } else {
            t.checkpoints.snapshot_intent = true
        }
        self.store.save_transition(id, t)?;
        match self.backend.action(resource, kind, name).await? {
            Mutation::Confirmed(a) => {
                if shutdown {
                    t.checkpoints.shutdown_action = Some(a.id.clone())
                } else {
                    t.checkpoints.snapshot_action = Some(a.id.clone())
                };
                self.store.save_transition(id, t)?;
                Ok(a)
            }
            Mutation::Uncertain { diagnostic } => {
                let found = self
                    .backend
                    .find_actions(resource, kind, &t.started_at)
                    .await?;
                if found.len() != 1 {
                    return Err(Error::Uncertain(format!(
                        "{diagnostic}; {} matching actions",
                        found.len()
                    )));
                }
                if shutdown {
                    t.checkpoints.shutdown_action = Some(found[0].id.clone())
                } else {
                    t.checkpoints.snapshot_action = Some(found[0].id.clone())
                };
                self.store.save_transition(id, t)?;
                Ok(found[0].clone())
            }
            Mutation::Rejected { diagnostic } => Err(Error::Backend(diagnostic)),
        }
    }
    async fn continue_power_off(
        &self,
        t: &mut Transition,
        id: &str,
        resource: &str,
        confirm: bool,
    ) -> Result<Action> {
        if let Some(action_id) = &t.checkpoints.power_off_action {
            return self.backend.get_action(action_id).await?.ok_or_else(|| {
                Error::Uncertain(format!("persisted action {action_id} is missing"))
            });
        }
        if t.checkpoints.power_off_intent {
            let found = self
                .backend
                .find_actions(resource, "power_off", &t.started_at)
                .await?;
            if found.len() == 1 {
                t.checkpoints.power_off_action = Some(found[0].id.clone());
                self.store.save_transition(id, t)?;
                return Ok(found[0].clone());
            }
            if !found.is_empty() {
                return Err(Error::Uncertain(format!(
                    "{} matching power_off actions found",
                    found.len()
                )));
            }
            if !confirm {
                console::action(
                    "Confirmation",
                    "Verify no power-off action exists, then rerun with --confirm-request-not-accepted",
                );
                return Err(Error::Uncertain(
                    "no matching power-off action found; explicit confirmation is required before resubmission"
                        .into(),
                ));
            }
            t.checkpoints.power_off_intent = false;
            self.store.save_transition(id, t)?;
            console::success(
                "Confirmation",
                "Prior power-off request confirmed absent; resubmitting",
            );
        }
        t.checkpoints.power_off_intent = true;
        self.store.save_transition(id, t)?;
        match self.backend.action(resource, "power_off", None).await? {
            Mutation::Confirmed(a) => {
                t.checkpoints.power_off_action = Some(a.id.clone());
                self.store.save_transition(id, t)?;
                Ok(a)
            }
            Mutation::Uncertain { diagnostic } => Err(Error::Uncertain(diagnostic)),
            Mutation::Rejected { diagnostic } => Err(Error::Backend(diagnostic)),
        }
    }
    pub async fn resume(&self, id: &str, options: ResumeOptions) -> Result<Instance> {
        console::heading("Resume", id);
        let _lock = self.store.lock_existing(id)?;
        let mut i = self.store.load_or_adopt_locked(id)?;
        let existing = self.store.transition(id)?;
        if let Some(t) = &existing {
            if t.kind == TransitionKind::Resume
                && t.phase == Phase::ActiveSnapshotCleanupPending
                && matches!(
                    i.lifecycle,
                    Lifecycle::Active {
                        snapshot: Some(_),
                        ..
                    }
                )
            {
                return self.cleanup_snapshot(id, i, options).await;
            }
            if t.kind != TransitionKind::Resume {
                return Err(Error::State(
                    "a non-resume transition is already in progress".into(),
                ));
            }
        }
        let snap = existing
            .as_ref()
            .and_then(|t| t.snapshot.clone())
            .or_else(|| match &i.lifecycle {
                Lifecycle::Paused { snapshot } => Some(snapshot.clone()),
                _ => None,
            })
            .ok_or_else(|| Error::State("resume requires paused state".into()))?;
        match &i.lifecycle {
            Lifecycle::Paused { snapshot } => snapshot.clone(),
            Lifecycle::Active {
                snapshot: Some(_), ..
            } => return self.cleanup_snapshot(id, i, options).await,
            _ if existing.is_none() => {
                return Err(Error::State("resume requires paused state".into()));
            }
            _ => snap.clone(),
        };
        let d = &self.config.backends.digitalocean;
        let correlation = format!("vps-resume-{}", Uuid::new_v4());
        let source_recipe = snap
            .source_recipe
            .clone()
            .ok_or_else(|| Error::State("paused state lacks an exact source recipe".into()))?;
        console::pending("Snapshot", "Verifying the retained recovery snapshot");
        let current_snapshot = self.backend.get_snapshot(&snap.id).await?.ok_or_else(|| {
            if options.confirm_missing_snapshot {
                Error::State("confirmed missing snapshot cannot be resumed".into())
            } else {
                Error::Uncertain("paused snapshot is missing".into())
            }
        })?;
        validate_snapshot(
            &current_snapshot,
            &snap.id,
            &snap.name,
            &snap.source_id,
            &source_recipe.region,
            snap.min_disk_gb,
        )?;
        console::success("Snapshot", "Recovery snapshot verified");
        let mut recipe = CreateRecipe {
            name: source_recipe.name,
            region: source_recipe.region,
            size: source_recipe.size,
            image: snap.id.clone(),
            tags: source_recipe.tags,
            ssh_key: d
                .ssh_key
                .clone()
                .ok_or_else(|| Error::Cli("SSH key missing".into()))?,
        };
        recipe.tags.push(correlation.clone());
        let mut t = existing.unwrap_or(Transition {
            schema_version: 1,
            kind: TransitionKind::Resume,
            phase: Phase::ResumingAllocation,
            operation_id: Uuid::new_v4().to_string(),
            started_at: now(),
            checkpoints: Default::default(),
            source: None,
            snapshot: Some(snap.clone()),
            target_recipe: Some(recipe.clone()),
            target: None,
            correlation: Some(correlation.clone()),
        });
        t.validate(&i)?;
        let recipe = t
            .target_recipe
            .clone()
            .ok_or_else(|| Error::State("resume target recipe missing".into()))?;
        let correlation = t
            .correlation
            .clone()
            .ok_or_else(|| Error::State("resume correlation missing".into()))?;
        let server = if let Some(target) = t.target.clone() {
            console::pending("Provider", "Resuming the persisted replacement Droplet");
            target
        } else {
            console::pending("Provider", "Creating the replacement Droplet");
            if t.checkpoints.target_create_intent {
                let m = self.backend.find_servers(&correlation).await?;
                if m.len() != 1 {
                    if m.is_empty() && options.confirm_request_not_accepted {
                        t.checkpoints.target_create_intent = false;
                        self.store.save_transition(id, &t)?;
                        return Err(Error::Uncertain(
                            "cleared unaccepted replacement create intent; rerun to submit it"
                                .into(),
                        ));
                    }
                    return Err(Error::Uncertain(format!(
                        "persisted replacement intent has {} matches; refusing another create",
                        m.len()
                    )));
                }
                m[0].clone()
            } else {
                t.checkpoints.target_create_intent = true;
                self.store.save_transition(id, &t)?;
                match self.backend.create_server(&recipe, None).await? {
                    Mutation::Confirmed(_) => {
                        let m = self.backend.find_servers(&correlation).await?;
                        if m.len() != 1 {
                            return Err(Error::Uncertain(format!(
                                "replacement correlation has {} matches",
                                m.len()
                            )));
                        }
                        m[0].clone()
                    }
                    Mutation::Uncertain { .. } => {
                        let m = self.backend.find_servers(&correlation).await?;
                        if m.len() != 1 {
                            return Err(Error::Uncertain(format!(
                                "replacement correlation has {} matches",
                                m.len()
                            )));
                        }
                        m[0].clone()
                    }
                    Mutation::Rejected { diagnostic } => return Err(Error::Backend(diagnostic)),
                }
            }
        };
        // Correlation discovery may return a target before networking is assigned. Refresh
        // only the exact discovered ID and persist the resulting immutable inventory.
        let mut server = server;
        console::pending("Provider", "Waiting for replacement public networking");
        for _ in 0..60 {
            server = self.backend.get_server(&server.id).await?.ok_or_else(|| {
                Error::Uncertain("correlated replacement disappeared by exact ID".into())
            })?;
            if server.endpoint.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
        if server.endpoint.is_none() {
            return Err(Error::Uncertain("replacement has no endpoint".into()));
        }
        if server.name != recipe.name
            || server.region != recipe.region
            || server.size != recipe.size
            || server.image != recipe.image
            || !server.tags.iter().any(|x| x == &correlation)
        {
            return Err(Error::State(
                "replacement does not match persisted recipe".into(),
            ));
        }
        t.target = Some(server.clone());
        t.phase = Phase::ResumingRecovery;
        self.store.save_transition(id, &t)?;
        console::success("Provider", "Replacement Droplet has public networking");
        let known = self.store.dir(id).join("known_hosts");
        let endpoint = server
            .endpoint
            .as_deref()
            .ok_or_else(|| Error::Uncertain("replacement has no endpoint".into()))?;
        secure_write(&known, format!("{endpoint} {}\n", snap.host_key).as_bytes())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if fs::metadata(&known)?.permissions().mode() & 0o777 != 0o600 {
                return Err(Error::State(
                    "resume known-hosts file is not mode 0600".into(),
                ));
            }
        }
        let key = key(self.config)?;
        console::pending("Worker", "Waiting for SSH on the replacement Droplet");
        let mut last_ssh_error = None;
        for _ in 0..60 {
            match ssh::stdin(&server, key, &known, "true", &[], false) {
                Ok(_) => {
                    last_ssh_error = None;
                    break;
                }
                Err(error) => {
                    last_ssh_error = Some(error);
                    tokio::time::sleep(Duration::from_secs(5)).await;
                }
            }
        }
        if let Some(error) = last_ssh_error {
            return Err(error);
        }
        console::success("Worker", "SSH is ready");
        console::pending("Worker", "Verifying snapshot identity and SSH host key");
        let actual_key = ssh::capture(
            &server,
            key,
            &known,
            "cat /etc/ssh/ssh_host_ed25519_key.pub",
        )?;
        let key_identity = |v: &str| v.split_whitespace().take(2).collect::<Vec<_>>().join(" ");
        if key_identity(&actual_key) != key_identity(&snap.host_key)
            || !key_identity(&actual_key).starts_with("ssh-ed25519 ")
        {
            return Err(Error::Remote(
                "resumed SSH Ed25519 key does not equal the captured key".into(),
            ));
        }
        let marker_text = ssh::capture(
            &server,
            key,
            &known,
            "cat /home/agent/.config/vps-codex/pause.env",
        )?;
        let marker = parse_pause_marker(&marker_text)?;
        if marker.operation_id != snap.pause_operation_id
            || key_identity(&marker.host_key) != key_identity(&snap.host_key)
        {
            return Err(Error::Remote(
                "pause marker operation or host key does not match the snapshot".into(),
            ));
        }
        console::success("Worker", "Snapshot identity and SSH host key verified");
        let running = marker
            .running_container_ids
            .iter()
            .map(|v| shq(v))
            .collect::<Vec<_>>()
            .join(" ");
        let healthy = marker
            .healthy_container_ids
            .iter()
            .map(|v| shq(v))
            .collect::<Vec<_>>()
            .join(" ");
        let recovery = format!(
            r#"set -euo pipefail
test -f /opt/codex-worker-ready
cloud-init status --wait >/dev/null
docker info >/dev/null
running=({running}); healthy=({healthy})
for id in "${{running[@]}}"; do docker inspect "$id" >/dev/null; docker start "$id" >/dev/null; done
for id in "${{running[@]}}"; do [[ "$(docker inspect -f '{{{{.State.Running}}}}' "$id")" == true ]]; done
for attempt in {{1..90}}; do
  bad=0; for id in "${{healthy[@]}}"; do [[ "$(docker inspect -f '{{{{.State.Health.Status}}}}' "$id")" == healthy ]] || bad=1; done
  (( bad == 0 )) && break; (( attempt < 90 )); sleep 5
done
checkout={checkout}; origin={origin}; branch={branch}
[[ -d "$checkout/.git" ]]
[[ "$(git -C "$checkout" remote get-url origin)" == "$origin" ]]
[[ "$(git -C "$checkout" symbolic-ref --short HEAD)" == "$branch" ]]
"#,
            checkout = shq(&format!(
                "/home/agent/projects/{}",
                i.repository.split('/').next_back().unwrap()
            )),
            origin = shq(&format!("https://github.com/{}.git", i.repository)),
            branch = shq(&i.work_branch),
        );
        console::pending("Worker", "Restarting containers and verifying the checkout");
        ssh::stdin(&server, key, &known, "bash -s", recovery.as_bytes(), false)?;
        console::success("Worker", "Containers and checkout recovered");

        let dir = self.store.dir(id);
        for ownership in ["chatgpt-login.json", "remote-control.json"] {
            if !dir.join(ownership).is_file() {
                return Err(Error::State(format!(
                    "local remote-control ownership marker {ownership} is missing"
                )));
            }
        }
        let actor = self.install_github_token(&i, &server, &known).await?;
        let token = crate::github::retained_token(&dir)?
            .ok_or_else(|| Error::State("GitHub token disappeared after installation".into()))?;
        let verify_token = r#"set -euo pipefail
candidate=$(mktemp); trap 'rm -f "$candidate"' EXIT; cat >"$candidate"
cmp -s "$candidate" /home/agent/.config/vps-codex/github-token
GH_TOKEN="$(cat "$candidate")" /usr/local/bin/gh auth status >/dev/null
"#;
        ssh::stdin(
            &server,
            key,
            &known,
            &format!("bash -c {}", shq(verify_token)),
            token.as_bytes(),
            false,
        )?;
        if !chatgpt_logged_in(&server, key, &known)? {
            return Err(Error::Remote(
                "snapshotted ChatGPT login is unavailable".into(),
            ));
        }
        console::success("GitHub", format!("Authenticated as {actor}"));
        console::pending(
            "Remote control",
            "Reconnecting with the preserved host enrollment",
        );
        let start: serde_json::Value = serde_json::from_str(&ssh::capture(
            &server,
            key,
            &known,
            "/usr/local/bin/codex remote-control --json start",
        )?)?;
        if !remote_usable(&start) {
            return Err(Error::Remote("remote control did not become usable".into()));
        }
        console::success(
            "Remote control",
            "Reconnected using the preserved host enrollment",
        );
        t.checkpoints.recovery_verified_at = Some(now());
        t.phase = Phase::ActiveSnapshotCleanupPending;
        self.store.save_transition(id, &t)?;
        i.lifecycle = Lifecycle::Active {
            server,
            snapshot: Some(snap),
        };
        self.store.save(&i)?;
        self.cleanup_snapshot(id, i, options).await
    }
    async fn cleanup_snapshot(
        &self,
        id: &str,
        mut i: Instance,
        options: ResumeOptions,
    ) -> Result<Instance> {
        let (server, snap) = match i.lifecycle.clone() {
            Lifecycle::Active {
                server,
                snapshot: Some(s),
            } => (server, s),
            _ => return Ok(i),
        };
        let mut t = self.store.transition(id)?.ok_or_else(|| {
            Error::State("refusing snapshot cleanup without its recovery transition".into())
        })?;
        if t.checkpoints.recovery_verified_at.is_none() {
            return Err(Error::State(
                "refusing snapshot cleanup before a recovery decision".into(),
            ));
        }
        t.checkpoints.snapshot_delete_intent = true;
        self.store.save_transition(id, &t)?;
        console::pending("Snapshot", "Deleting the consumed recovery snapshot");
        if let Some(found) = self.backend.get_snapshot(&snap.id).await? {
            let source_region = snap
                .source_recipe
                .as_ref()
                .map_or(snap.region.as_str(), |r| r.region.as_str());
            validate_snapshot(
                &found,
                &snap.id,
                &snap.name,
                &snap.source_id,
                source_region,
                snap.min_disk_gb,
            )?;
            confirmed(self.backend.delete_snapshot(&snap.id).await?)?;
        } else {
            if !options.confirm_missing_snapshot {
                return Err(Error::Uncertain("snapshot deletion is unresolved; rerun with --confirm-missing-snapshot after verifying absence".into()));
            }
        }
        if let Some(mut t) = self.store.transition(id)? {
            t.checkpoints.snapshot_delete_confirmed = true;
            self.store.save_transition(id, &t)?;
        }
        ssh::run(
            &server,
            key(self.config)?,
            &self.store.dir(id).join("known_hosts"),
            &["rm", "-f", "/home/agent/.config/vps-codex/pause.env"],
            false,
        )?;
        i.lifecycle = Lifecycle::Active {
            server,
            snapshot: None,
        };
        self.store.save(&i)?;
        self.store.clear_transition(id)?;
        console::success("Snapshot", "Consumed recovery snapshot deleted");
        Ok(i)
    }
    pub async fn destroy(&self, id: &str, options: DestroyOptions) -> Result<()> {
        console::heading("Destroy", id);
        let _lock = self.store.lock_existing(id)?;
        let i = self.store.load_or_adopt_locked(id)?;
        let existing = self.store.transition(id)?;
        if matches!(i.lifecycle, Lifecycle::SourceReserved { .. }) {
            if existing.is_some() {
                return Err(Error::State(
                    "source-reserved state unexpectedly has provider mutation evidence".into(),
                ));
            }
            for token in crate::github::retained_tokens(&self.store.dir(id))? {
                console::pending("GitHub", "Revoking the retained repository credential");
                if let Err(error) = crate::github::revoke(&token).await
                    && !options.forget_unrevoked_token
                {
                    return Err(error);
                }
                console::success("GitHub", "Repository credential revoked");
            }
            fs::remove_dir_all(self.store.dir(id))?;
            console::success("Local state", "Instance state removed");
            return Ok(());
        }
        console::pending(
            "Inventory",
            "Reconciling provider resources and pending operations",
        );
        let mut servers = Vec::<Server>::new();
        let mut snapshots = Vec::<Snapshot>::new();
        let mut correlations = Vec::<String>::new();
        match &i.lifecycle {
            Lifecycle::Active {
                server: s,
                snapshot: p,
            } => {
                servers.push(s.clone());
                if let Some(p) = p {
                    snapshots.push(p.clone())
                }
            }
            Lifecycle::Paused { snapshot: s } => snapshots.push(s.clone()),
            Lifecycle::AllocationPending { correlation, .. } => {
                correlations.push(correlation.clone());
            }
            Lifecycle::SourceReserved { .. } => unreachable!(),
        }
        if let Some(t) = &existing {
            if let Some(s) = &t.source {
                servers.push(s.clone())
            }
            if let Some(s) = &t.target {
                servers.push(s.clone())
            }
            if let Some(s) = &t.snapshot {
                snapshots.push(s.clone())
            }
            if t.kind == TransitionKind::Pause
                && t.checkpoints.snapshot_intent
                && t.snapshot.is_none()
            {
                let source = t.source.as_ref().ok_or_else(|| {
                    Error::State("pending snapshot has no source evidence".into())
                })?;
                let expected_name = format!("vps-{id}-{}", t.operation_id);
                let mut found: Vec<_> = self
                    .backend
                    .snapshots()
                    .await?
                    .into_iter()
                    .filter(|s| {
                        s.name == expected_name
                            && s.source_id == source.id
                            && s.regions.iter().any(|region| region == &source.region)
                    })
                    .collect();
                match found.len() {
                    1 => {
                        let snapshot = found.remove(0);
                        let direct =
                            self.backend
                                .get_snapshot(&snapshot.id)
                                .await?
                                .ok_or_else(|| {
                                    Error::Uncertain(
                                        "pending pause snapshot disappeared before verification"
                                            .into(),
                                    )
                                })?;
                        validate_snapshot(
                            &direct,
                            &snapshot.id,
                            &expected_name,
                            &source.id,
                            &source.region,
                            source.disk_gb,
                        )?;
                        let mut direct = direct;
                        direct.region = source.region.clone();
                        snapshots.push(direct);
                    }
                    0 if options.confirm_request_not_accepted => {}
                    0 => {
                        return Err(Error::Uncertain(
                            "pending pause snapshot request has no exact match; state retained"
                                .into(),
                        ));
                    }
                    n => {
                        return Err(Error::Uncertain(format!(
                            "pending pause snapshot has {n} exact matches; state retained"
                        )));
                    }
                }
            }
            if t.checkpoints.target_create_intent && t.target.is_none() {
                correlations.push(t.correlation.clone().ok_or_else(|| {
                    Error::State("target create intent has no correlation".into())
                })?);
            }
        }
        for correlation in correlations {
            let found = self.backend.find_servers(&correlation).await?;
            if found.len() > 1 {
                return Err(Error::Uncertain(format!(
                    "correlation {correlation} has multiple servers; refusing partial teardown"
                )));
            }
            if found.is_empty() && !options.forget_unresolved_allocation {
                return Err(Error::Uncertain(format!(
                    "correlation {correlation} has no visible server; rerun with --forget-unresolved-allocation only after confirming no allocation exists"
                )));
            }
            servers.extend(found);
        }
        servers.sort_by(|a, b| a.id.cmp(&b.id));
        servers.dedup_by(|a, b| a.id == b.id);
        snapshots.sort_by(|a, b| a.id.cmp(&b.id));
        snapshots.dedup_by(|a, b| a.id == b.id);
        console::success(
            "Inventory",
            format!(
                "Reconciled {} server(s) and {} snapshot(s)",
                servers.len(),
                snapshots.len()
            ),
        );
        // Persist the complete inventory and teardown decision before the first mutation.
        let teardown = Transition {
            schema_version: 1,
            kind: TransitionKind::Destroy,
            phase: Phase::Destroying,
            operation_id: existing
                .as_ref()
                .map(|t| t.operation_id.clone())
                .unwrap_or_else(|| Uuid::new_v4().to_string()),
            started_at: existing
                .as_ref()
                .map(|t| t.started_at.clone())
                .unwrap_or_else(now),
            checkpoints: Checkpoints {
                teardown_started: true,
                ..Default::default()
            },
            source: servers.first().cloned(),
            target: servers.get(1).cloned(),
            snapshot: snapshots.first().cloned(),
            target_recipe: existing.as_ref().and_then(|t| t.target_recipe.clone()),
            correlation: existing.as_ref().and_then(|t| t.correlation.clone()),
        };
        self.store.save_transition(id, &teardown)?;
        for s in &servers {
            console::pending("Provider", "Deleting the provider server");
            if self.backend.get_server(&s.id).await?.is_none() {
                if !options.confirm_missing_server {
                    return Err(Error::Uncertain(format!(
                        "server {} is missing; explicit confirmation is required",
                        s.id
                    )));
                }
            } else {
                confirmed(self.backend.delete_server(&s.id).await?)?;
            }
            console::success("Provider", "Provider server deletion confirmed");
        }
        // Credentials are revoked only after every inventoried server has confirmed deletion.
        for token in crate::github::retained_tokens(&self.store.dir(id))? {
            console::pending("GitHub", "Revoking the retained repository credential");
            if let Err(e) = crate::github::revoke(&token).await
                && !options.forget_unrevoked_token
            {
                return Err(e);
            }
            console::success("GitHub", "Repository credential revoked");
        }
        for s in &snapshots {
            console::pending("Snapshot", "Deleting the retained snapshot");
            let direct = self.backend.get_snapshot(&s.id).await?;
            if let Some(ref found) = direct {
                let source_region = s
                    .source_recipe
                    .as_ref()
                    .map_or(s.region.as_str(), |r| r.region.as_str());
                validate_snapshot(
                    found,
                    &s.id,
                    &s.name,
                    &s.source_id,
                    source_region,
                    s.min_disk_gb,
                )?;
            }
            if direct.is_none() {
                if !options.confirm_missing_snapshot {
                    return Err(Error::Uncertain(format!(
                        "snapshot {} is missing; explicit confirmation is required",
                        s.id
                    )));
                }
            } else {
                confirmed(self.backend.delete_snapshot(&s.id).await?)?;
            }
            console::success("Snapshot", "Retained snapshot deletion confirmed");
        }
        fs::remove_dir_all(self.store.dir(id))?;
        console::success("Local state", "Instance state removed");
        Ok(())
    }
}
fn confirmed<T>(m: Mutation<T>) -> Result<T> {
    match m {
        Mutation::Confirmed(v) => Ok(v),
        Mutation::Rejected { diagnostic } => Err(Error::Backend(diagnostic)),
        Mutation::Uncertain { diagnostic } => Err(Error::Uncertain(diagnostic)),
    }
}
fn key(c: &Config) -> Result<&Path> {
    c.ssh
        .private_key
        .as_deref()
        .ok_or_else(|| Error::Cli("ssh.private_key is required".into()))
}
fn public_key(private: Option<&Path>) -> Result<String> {
    let p = private.ok_or_else(|| Error::Cli("ssh.private_key is required".into()))?;
    let pubp = std::path::PathBuf::from(format!("{}.pub", p.display()));
    let s = fs::read_to_string(&pubp)
        .map_err(|_| Error::Cli(format!("public SSH key not found: {}", pubp.display())))?;
    if !s.starts_with("ssh-") {
        return Err(Error::Cli("public SSH key is malformed".into()));
    }
    Ok(s.trim().into())
}

fn secure_write(path: &Path, contents: &[u8]) -> Result<()> {
    use std::io::Write;
    let temporary = path.with_extension(format!("tmp-{}", Uuid::new_v4()));
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temporary)?;
    file.write_all(contents)?;
    file.sync_all()?;
    fs::rename(temporary, path)?;
    Ok(())
}
