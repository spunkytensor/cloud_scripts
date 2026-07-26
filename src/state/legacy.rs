use crate::{
    error::{Error, Result},
    model::*,
};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::Path,
};

pub struct Import {
    pub instance: Instance,
    pub transition: Option<Transition>,
    pub credentials_owned: bool,
}

pub fn parse_file(path: &Path) -> Result<BTreeMap<String, String>> {
    parse(&fs::read_to_string(path)?)
}
pub fn parse(s: &str) -> Result<BTreeMap<String, String>> {
    let mut out = BTreeMap::new();
    for (n, raw) in s.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (k, v) = line
            .split_once('=')
            .ok_or_else(|| Error::State(format!("legacy line {} is not an assignment", n + 1)))?;
        if k.is_empty()
            || !k
                .bytes()
                .all(|c| c.is_ascii_uppercase() || c == b'_' || c.is_ascii_digit())
        {
            return Err(Error::State(format!(
                "invalid legacy key at line {}",
                n + 1
            )));
        }
        if out.contains_key(k) {
            return Err(Error::State(format!("duplicate legacy key {k}")));
        }
        out.insert(
            k.into(),
            decode_q(v).map_err(|e| Error::State(format!("legacy line {}: {e}", n + 1)))?,
        );
    }
    Ok(out)
}
fn decode_q(s: &str) -> std::result::Result<String, String> {
    if s.is_empty() {
        return Ok(String::new());
    }
    if s.starts_with("$'") && s.ends_with('\'') {
        let mut o = String::new();
        let mut c = s[2..s.len() - 1].chars();
        while let Some(x) = c.next() {
            if x != '\\' {
                o.push(x);
                continue;
            }
            match c.next().ok_or("trailing escape")? {
                'n' => o.push('\n'),
                'r' => o.push('\r'),
                't' => o.push('\t'),
                '\'' => o.push('\''),
                '\\' => o.push('\\'),
                x => return Err(format!("unsupported escape \\{x}")),
            }
        }
        return Ok(o);
    }
    if s.starts_with('\'') && s.ends_with('\'') {
        return Ok(s[1..s.len() - 1].into());
    }
    let mut o = String::new();
    let mut esc = false;
    for c in s.chars() {
        if esc {
            o.push(c);
            esc = false
        } else if c == '\\' {
            esc = true
        } else if c.is_whitespace()
            || matches!(c, ';' | '&' | '|' | '`' | '$' | '(' | ')' | '<' | '>')
        {
            return Err("unsafe unescaped shell metacharacter".into());
        } else {
            o.push(c)
        }
    }
    if esc {
        return Err("trailing escape".into());
    }
    Ok(o)
}
fn file(dir: &Path, suffix: &str, allowed: &[&str]) -> Result<Option<BTreeMap<String, String>>> {
    let p = dir.join(format!("current.env{suffix}"));
    if !p.exists() {
        return Ok(None);
    }
    let m = parse_file(&p)?;
    let a: BTreeSet<_> = allowed.iter().copied().collect();
    if let Some(k) = m.keys().find(|k| !a.contains(k.as_str())) {
        return Err(Error::State(format!(
            "unknown field {k} in {}",
            p.display()
        )));
    }
    Ok(Some(m))
}
fn req(m: &BTreeMap<String, String>, k: &str) -> Result<String> {
    m.get(k)
        .filter(|v| !v.is_empty())
        .cloned()
        .ok_or_else(|| Error::State(format!("legacy field {k} missing")))
}
fn boolean(m: &BTreeMap<String, String>, k: &str) -> Result<bool> {
    match m.get(k).map(String::as_str).unwrap_or("") {
        "" | "0" => Ok(false),
        "1" => Ok(true),
        _ => Err(Error::State(format!("invalid legacy boolean {k}"))),
    }
}
fn tags(v: Option<&String>) -> Vec<String> {
    v.map(|x| {
        x.split(',')
            .filter(|x| !x.is_empty())
            .map(str::to_owned)
            .collect()
    })
    .unwrap_or_default()
}
fn recipe(
    name: String,
    region: String,
    size: String,
    image: String,
    tags: Vec<String>,
) -> CreateRecipe {
    CreateRecipe {
        name,
        region,
        size,
        image,
        tags,
        ssh_key: "legacy-configured-key".into(),
    }
}
fn server(m: &BTreeMap<String, String>, prefix: bool) -> Result<Server> {
    let p = if prefix { "SOURCE_" } else { "TARGET_" };
    Ok(Server {
        id: req(m, &format!("{p}DROPLET_ID"))?,
        name: req(m, &format!("{p}DROPLET_NAME"))?,
        endpoint: m
            .get(&format!("{p}DROPLET_IP"))
            .filter(|x| !x.is_empty())
            .cloned(),
        region: req(m, &format!("{p}REGION"))?,
        size: req(m, &format!("{p}SIZE"))?,
        image: "legacy".into(),
        tags: tags(m.get(&format!("{p}TAGS"))),
        disk_gb: m
            .get("SOURCE_DISK_SIZE")
            .and_then(|x| x.parse().ok())
            .unwrap_or(0),
        status: String::new(),
        volume_ids: vec![],
    })
}
const SETUP: &[&str] = &[
    "SETUP_REPOSITORY",
    "SETUP_BASE_BRANCH",
    "SETUP_WORK_BRANCH",
    "SETUP_INSTANCE_ID",
    "SETUP_CREDENTIAL_ISOLATION_VERSION",
];
const ACTIVE: &[&str] = &[
    "DROPLET_ID",
    "DROPLET_IP",
    "DROPLET_NAME",
    "KNOWN_HOSTS_FILE",
    "SSH_CONFIG_FILE",
    "SSH_ALIAS",
    "CREATED_AT",
];
const PAUSED: &[&str] = &[
    "PAUSED_STATE_VERSION",
    "PAUSE_OPERATION_ID",
    "SNAPSHOT_ID",
    "SNAPSHOT_NAME",
    "SNAPSHOT_SOURCE_DROPLET_ID",
    "SNAPSHOT_CREATED_AT",
    "DROPLET_NAME",
    "DROPLET_REGION",
    "DROPLET_SIZE",
    "DROPLET_TAGS",
    "SOURCE_DISK_SIZE",
    "SSH_HOST_ED25519_PUBLIC_KEY",
    "PAUSED_AT",
];
const ALLOC: &[&str] = &[
    "ALLOCATION_DROPLET_NAME",
    "ALLOCATION_REGION",
    "ALLOCATION_SIZE",
    "ALLOCATION_IMAGE",
    "ALLOCATION_TAGS",
    "ALLOCATION_LIFECYCLE_TAG",
    "ALLOCATION_REQUESTED",
];
const TRANS: &[&str] = &[
    "TRANSITION_VERSION",
    "TRANSITION_KIND",
    "TRANSITION_PHASE",
    "OPERATION_ID",
    "PAUSE_OPERATION_ID",
    "TRANSITION_STARTED_AT",
    "SOURCE_DROPLET_ID",
    "SOURCE_DROPLET_NAME",
    "SOURCE_DROPLET_IP",
    "SOURCE_REGION",
    "SOURCE_SIZE",
    "SOURCE_TAGS",
    "SOURCE_DISK_SIZE",
    "REMOTE_QUIESCE_VERIFIED",
    "SHUTDOWN_REQUESTED",
    "SHUTDOWN_ACTION_ID",
    "POWER_OFF_REQUESTED",
    "POWER_OFF_ACTION_ID",
    "SNAPSHOT_NAME",
    "SNAPSHOT_REQUESTED",
    "SNAPSHOT_ACTION_ID",
    "SNAPSHOT_ID",
    "SNAPSHOT_CREATED_AT",
    "SOURCE_DELETE_REQUESTED",
    "SOURCE_DELETE_CONFIRMED",
    "SSH_HOST_ED25519_PUBLIC_KEY",
    "TARGET_DROPLET_NAME",
    "TARGET_REGION",
    "TARGET_SIZE",
    "TARGET_TAGS",
    "TARGET_LIFECYCLE_TAG",
    "TARGET_CREATE_REQUESTED",
    "TARGET_DROPLET_ID",
    "TARGET_DROPLET_IP",
    "RECOVERY_VERIFIED_AT",
    "SNAPSHOT_DELETE_REQUESTED",
    "SNAPSHOT_DELETE_CONFIRMED",
    "TEARDOWN_STARTED",
    "TARGET_DELETE_CONFIRMED",
];

pub fn import(dir: &Path, id: &str) -> Result<Import> {
    validate_instance_id(id)?;
    let setup =
        file(dir, ".setup", SETUP)?.ok_or_else(|| Error::State("legacy setup missing".into()))?;
    let active = file(dir, "", ACTIVE)?;
    let paused = file(dir, ".paused", PAUSED)?;
    let alloc = file(dir, ".allocation", ALLOC)?;
    let tr = file(dir, ".transition", TRANS)?;
    if active.is_some() && paused.is_some() {
        return Err(Error::State(
            "contradictory active and paused terminal state".into(),
        ));
    }
    if alloc.is_some() && (active.is_some() || paused.is_some()) {
        return Err(Error::State("allocation contradicts terminal state".into()));
    }
    let repository = req(&setup, "SETUP_REPOSITORY")?;
    let base_branch = req(&setup, "SETUP_BASE_BRANCH")?;
    validate_repository(&repository)?;
    validate_branch(&base_branch)?;
    let work_branch = setup
        .get("SETUP_WORK_BRANCH")
        .filter(|x| !x.is_empty())
        .cloned()
        .unwrap_or_else(|| format!("codex/{id}"));
    let credentials_owned = setup
        .get("SETUP_CREDENTIAL_ISOLATION_VERSION")
        .map(String::as_str)
        == Some("1")
        && setup.get("SETUP_INSTANCE_ID").map(String::as_str) == Some(id);
    let token_bearing = dir.join("current.env.github-token").is_file()
        || dir.join("current.env.github-token.replacement").is_file();
    if token_bearing && !credentials_owned {
        return Err(Error::State(
            "legacy token state lacks matching credential-isolation ownership".into(),
        ));
    }
    let paused_snapshot = |p: &BTreeMap<String, String>| -> Result<Snapshot> {
        Ok(Snapshot {
            id: req(p, "SNAPSHOT_ID")?,
            name: req(p, "SNAPSHOT_NAME")?,
            source_id: req(p, "SNAPSHOT_SOURCE_DROPLET_ID")?,
            region: req(p, "DROPLET_REGION")?,
            min_disk_gb: p
                .get("SOURCE_DISK_SIZE")
                .and_then(|x| x.parse().ok())
                .unwrap_or(0),
            host_key: req(p, "SSH_HOST_ED25519_PUBLIC_KEY")?,
            pause_operation_id: req(p, "PAUSE_OPERATION_ID")?,
            source_recipe: Some(recipe(
                req(p, "DROPLET_NAME")?,
                req(p, "DROPLET_REGION")?,
                req(p, "DROPLET_SIZE")?,
                "legacy".into(),
                tags(p.get("DROPLET_TAGS")),
            )),
            regions: vec![req(p, "DROPLET_REGION")?],
        })
    };
    let lifecycle = if let Some(a) = &active {
        Lifecycle::Active {
            server: Server {
                id: req(a, "DROPLET_ID")?,
                name: req(a, "DROPLET_NAME")?,
                endpoint: a.get("DROPLET_IP").filter(|x| !x.is_empty()).cloned(),
                region: "legacy".into(),
                size: "legacy".into(),
                image: "legacy".into(),
                tags: vec![],
                disk_gb: 0,
                status: String::new(),
                volume_ids: vec![],
            },
            snapshot: None,
        }
    } else if let Some(p) = &paused {
        Lifecycle::Paused {
            snapshot: paused_snapshot(p)?,
        }
    } else if let Some(a) = &alloc {
        Lifecycle::AllocationPending {
            recipe: recipe(
                req(a, "ALLOCATION_DROPLET_NAME")?,
                req(a, "ALLOCATION_REGION")?,
                req(a, "ALLOCATION_SIZE")?,
                req(a, "ALLOCATION_IMAGE")?,
                tags(a.get("ALLOCATION_TAGS")),
            ),
            correlation: req(a, "ALLOCATION_LIFECYCLE_TAG")?,
            request_intent: boolean(a, "ALLOCATION_REQUESTED")?,
        }
    } else {
        Lifecycle::SourceReserved {
            recipe: recipe(
                "legacy".into(),
                "legacy".into(),
                "legacy".into(),
                "legacy".into(),
                vec![],
            ),
        }
    };
    let mut instance = Instance {
        schema_version: 1,
        instance_id: id.into(),
        backend: "digitalocean".into(),
        created_at: active
            .as_ref()
            .and_then(|x| x.get("CREATED_AT"))
            .cloned()
            .unwrap_or_else(now),
        repository,
        base_branch,
        work_branch,
        lifecycle,
        provider: serde_json::json!({"legacy":true}),
    };
    let transition = tr.map(|m| transition(&m, &instance)).transpose()?;
    if let Some(t) = &transition
        && t.phase == Phase::ActiveSnapshotCleanupPending
    {
        let server = match &instance.lifecycle {
            Lifecycle::Active { server, .. } => server.clone(),
            _ => {
                return Err(Error::State(
                    "cleanup transition requires active state".into(),
                ));
            }
        };
        instance.lifecycle = Lifecycle::Active {
            server,
            snapshot: t.snapshot.clone(),
        };
    }
    if let Some(t) = &transition {
        t.validate(&instance)?;
    }
    Ok(Import {
        instance,
        transition,
        credentials_owned,
    })
}
fn transition(m: &BTreeMap<String, String>, i: &Instance) -> Result<Transition> {
    if m.get("TRANSITION_VERSION").map(String::as_str) != Some("1") {
        return Err(Error::State("invalid transition version".into()));
    }
    let kind = match req(m, "TRANSITION_KIND")?.as_str() {
        "pause" => TransitionKind::Pause,
        "resume" => TransitionKind::Resume,
        _ => return Err(Error::State("invalid transition kind".into())),
    };
    let phase = match req(m, "TRANSITION_PHASE")?.as_str() {
        "pausing-quiescing" => Phase::PausingQuiescing,
        "pausing-shutdown" => Phase::PausingShutdown,
        "pausing-snapshot" => Phase::PausingSnapshot,
        "pausing-delete-pending" => Phase::PausingDeletePending,
        "resuming-allocation" => Phase::ResumingAllocation,
        "resuming-recovery" => Phase::ResumingRecovery,
        "active-snapshot-cleanup-pending" => Phase::ActiveSnapshotCleanupPending,
        _ => return Err(Error::State("invalid transition phase".into())),
    };
    let source = if matches!(kind, TransitionKind::Pause) {
        Some(server(m, true)?)
    } else {
        None
    };
    let snapshot = if m.get("SNAPSHOT_ID").is_some_and(|x| !x.is_empty()) {
        Some(Snapshot {
            id: req(m, "SNAPSHOT_ID")?,
            name: req(m, "SNAPSHOT_NAME")?,
            source_id: req(m, "SOURCE_DROPLET_ID")?,
            region: req(m, "SOURCE_REGION")?,
            min_disk_gb: m
                .get("SOURCE_DISK_SIZE")
                .and_then(|x| x.parse().ok())
                .unwrap_or(0),
            host_key: req(m, "SSH_HOST_ED25519_PUBLIC_KEY")?,
            pause_operation_id: m
                .get("PAUSE_OPERATION_ID")
                .filter(|x| !x.is_empty())
                .cloned()
                .unwrap_or_else(|| req(m, "OPERATION_ID").unwrap()),
            source_recipe: None,
            regions: vec![req(m, "SOURCE_REGION")?],
        })
    } else {
        None
    };
    let target_recipe = if matches!(kind, TransitionKind::Resume) {
        Some(recipe(
            req(m, "TARGET_DROPLET_NAME")?,
            req(m, "TARGET_REGION")?,
            req(m, "TARGET_SIZE")?,
            req(m, "SNAPSHOT_ID")?,
            tags(m.get("TARGET_TAGS")),
        ))
    } else {
        None
    };
    let target = if m.get("TARGET_DROPLET_ID").is_some_and(|x| !x.is_empty()) {
        Some(server(m, false)?)
    } else {
        None
    };
    let mut t = Transition {
        schema_version: 1,
        kind,
        phase,
        operation_id: req(m, "OPERATION_ID")?,
        started_at: req(m, "TRANSITION_STARTED_AT")?,
        checkpoints: Checkpoints {
            quiescence_verified: boolean(m, "REMOTE_QUIESCE_VERIFIED")?,
            captured_host_key: m
                .get("SSH_HOST_ED25519_PUBLIC_KEY")
                .filter(|x| !x.is_empty())
                .cloned(),
            shutdown_intent: boolean(m, "SHUTDOWN_REQUESTED")?,
            shutdown_action: m
                .get("SHUTDOWN_ACTION_ID")
                .filter(|x| !x.is_empty())
                .cloned(),
            power_off_intent: boolean(m, "POWER_OFF_REQUESTED")?,
            power_off_action: m
                .get("POWER_OFF_ACTION_ID")
                .filter(|x| !x.is_empty())
                .cloned(),
            snapshot_intent: boolean(m, "SNAPSHOT_REQUESTED")?,
            snapshot_action: m
                .get("SNAPSHOT_ACTION_ID")
                .filter(|x| !x.is_empty())
                .cloned(),
            source_delete_intent: boolean(m, "SOURCE_DELETE_REQUESTED")?,
            source_delete_confirmed: boolean(m, "SOURCE_DELETE_CONFIRMED")?,
            target_create_intent: boolean(m, "TARGET_CREATE_REQUESTED")?,
            recovery_verified_at: m
                .get("RECOVERY_VERIFIED_AT")
                .filter(|x| !x.is_empty())
                .cloned(),
            snapshot_delete_intent: boolean(m, "SNAPSHOT_DELETE_REQUESTED")?,
            snapshot_delete_confirmed: boolean(m, "SNAPSHOT_DELETE_CONFIRMED")?,
            target_delete_confirmed: boolean(m, "TARGET_DELETE_CONFIRMED")?,
            teardown_started: boolean(m, "TEARDOWN_STARTED")?,
        },
        source,
        snapshot,
        target_recipe,
        target,
        correlation: m
            .get("TARGET_LIFECYCLE_TAG")
            .filter(|x| !x.is_empty())
            .cloned(),
    };
    if t.checkpoints.teardown_started {
        t.kind = TransitionKind::Destroy;
        t.phase = Phase::Destroying;
    }
    if t.phase != Phase::ActiveSnapshotCleanupPending {
        t.validate(i)?;
    }
    Ok(t)
}
