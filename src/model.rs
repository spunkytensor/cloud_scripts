// SPDX-FileCopyrightText: 2026 Matt Curfman
// SPDX-License-Identifier: Apache-2.0

use crate::error::{Error, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Instance {
    pub schema_version: u8,
    pub instance_id: String,
    pub backend: String,
    pub created_at: String,
    pub repository: String,
    pub base_branch: String,
    pub work_branch: String,
    pub lifecycle: Lifecycle,
    pub provider: Value,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(clippy::large_enum_variant)]
#[serde(tag = "state", rename_all = "kebab-case")]
pub enum Lifecycle {
    SourceReserved {
        recipe: CreateRecipe,
    },
    AllocationPending {
        recipe: CreateRecipe,
        correlation: String,
        #[serde(default)]
        request_intent: bool,
    },
    Active {
        server: Server,
        snapshot: Option<Snapshot>,
    },
    Paused {
        snapshot: Snapshot,
    },
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateRecipe {
    pub name: String,
    pub region: String,
    pub size: String,
    pub image: String,
    pub tags: Vec<String>,
    pub ssh_key: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Server {
    pub id: String,
    pub name: String,
    pub endpoint: Option<String>,
    pub region: String,
    pub size: String,
    pub image: String,
    pub tags: Vec<String>,
    pub disk_gb: u64,
    /// Provider power state (for example `active` or `off`).
    #[serde(default)]
    pub status: String,
    /// IDs of attached block-storage volumes. Pause is only safe when empty.
    #[serde(default)]
    pub volume_ids: Vec<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    pub id: String,
    pub name: String,
    pub source_id: String,
    pub region: String,
    pub min_disk_gb: u64,
    pub host_key: String,
    pub pause_operation_id: String,
    /// Exact source allocation recipe; resume must not substitute current defaults.
    #[serde(default)]
    pub source_recipe: Option<CreateRecipe>,
    /// Every region in which the provider says the snapshot is usable.
    #[serde(default)]
    pub regions: Vec<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Action {
    pub id: String,
    pub kind: String,
    pub resource_id: String,
    pub status: ActionStatus,
    pub started_at: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum ActionStatus {
    InProgress,
    Completed,
    Errored,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Transition {
    pub schema_version: u8,
    pub kind: TransitionKind,
    pub phase: Phase,
    pub operation_id: String,
    pub started_at: String,
    #[serde(default)]
    pub checkpoints: Checkpoints,
    pub source: Option<Server>,
    pub snapshot: Option<Snapshot>,
    pub target_recipe: Option<CreateRecipe>,
    pub target: Option<Server>,
    pub correlation: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "kebab-case")]
pub enum TransitionKind {
    Pause,
    Resume,
    Destroy,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "kebab-case")]
pub enum Phase {
    PausingQuiescing,
    PausingShutdown,
    PausingSnapshot,
    PausingDeletePending,
    ResumingAllocation,
    ResumingRecovery,
    ActiveSnapshotCleanupPending,
    Destroying,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Checkpoints {
    pub quiescence_verified: bool,
    /// Host key captured while the source was still reachable and trusted.
    #[serde(default)]
    pub captured_host_key: Option<String>,
    pub shutdown_intent: bool,
    pub shutdown_action: Option<String>,
    pub power_off_intent: bool,
    pub power_off_action: Option<String>,
    pub snapshot_intent: bool,
    pub snapshot_action: Option<String>,
    pub source_delete_intent: bool,
    pub source_delete_confirmed: bool,
    pub target_create_intent: bool,
    pub recovery_verified_at: Option<String>,
    pub snapshot_delete_intent: bool,
    pub snapshot_delete_confirmed: bool,
    pub target_delete_confirmed: bool,
    pub teardown_started: bool,
    /// Complete provider inventory frozen before teardown starts.
    #[serde(default)]
    pub teardown_servers: Vec<Server>,
    #[serde(default)]
    pub teardown_snapshots: Vec<Snapshot>,
}

impl Transition {
    /// Returns whether this journal describes work already reflected in `instance`.
    pub fn completed_for(&self, instance: &Instance) -> bool {
        match self.kind {
            TransitionKind::Pause => {
                self.phase == Phase::PausingDeletePending
                    && self.checkpoints.source_delete_confirmed
                    && matches!(
                        (&instance.lifecycle, &self.snapshot),
                        (Lifecycle::Paused { snapshot: current }, Some(recorded))
                            if current.id == recorded.id
                                && current.source_id == recorded.source_id
                    )
            }
            TransitionKind::Resume => {
                self.phase == Phase::ActiveSnapshotCleanupPending
                    && self.checkpoints.snapshot_delete_confirmed
                    && matches!(
                        (&instance.lifecycle, &self.target),
                        (
                            Lifecycle::Active {
                                server: current,
                                snapshot: None,
                            },
                            Some(recorded),
                        ) if current.id == recorded.id
                    )
            }
            TransitionKind::Destroy => false,
        }
    }

    /// Validates this value's persisted invariants and rejects unsafe or contradictory state.
    pub fn validate(&self, instance: &Instance) -> Result<()> {
        if self.schema_version != 1 || self.operation_id.is_empty() || self.started_at.is_empty() {
            return Err(Error::State("invalid transition header".into()));
        }
        let phase_kind = matches!(
            (&self.kind, &self.phase),
            (
                TransitionKind::Pause,
                Phase::PausingQuiescing
                    | Phase::PausingShutdown
                    | Phase::PausingSnapshot
                    | Phase::PausingDeletePending
            ) | (
                TransitionKind::Resume,
                Phase::ResumingAllocation
                    | Phase::ResumingRecovery
                    | Phase::ActiveSnapshotCleanupPending
            ) | (TransitionKind::Destroy, Phase::Destroying)
        );
        if !phase_kind {
            return Err(Error::State("transition kind and phase contradict".into()));
        }
        match self.kind {
            TransitionKind::Pause
                if !(matches!(instance.lifecycle, Lifecycle::Active { snapshot: None, .. })
                    || self.completed_for(instance)) =>
            {
                return Err(Error::State(
                    "pause transition requires active source state".into(),
                ));
            }
            TransitionKind::Resume
                if !(matches!(
                    instance.lifecycle,
                    Lifecycle::Paused { .. }
                        | Lifecycle::Active {
                            snapshot: Some(_),
                            ..
                        }
                ) || self.completed_for(instance)) =>
            {
                return Err(Error::State(
                    "resume transition requires paused or cleanup-pending state".into(),
                ));
            }
            _ => {}
        }
        if matches!(self.kind, TransitionKind::Pause) && self.source.is_none() {
            return Err(Error::State(
                "pause transition has no source inventory".into(),
            ));
        }
        if matches!(self.kind, TransitionKind::Pause)
            && self.phase != Phase::PausingQuiescing
            && self
                .checkpoints
                .captured_host_key
                .as_deref()
                .unwrap_or("")
                .is_empty()
        {
            return Err(Error::State(
                "pause transition has no captured host key".into(),
            ));
        }
        if matches!(self.kind, TransitionKind::Resume)
            && (self.snapshot.is_none()
                || self.target_recipe.is_none()
                || self.correlation.as_deref().unwrap_or("").is_empty())
        {
            return Err(Error::State(
                "resume transition is missing its immutable recipe".into(),
            ));
        }
        Ok(())
    }
}

impl Instance {
    /// Validates this value's persisted invariants and rejects unsafe or contradictory state.
    pub fn validate(&self) -> Result<()> {
        validate_instance_id(&self.instance_id)?;
        validate_repository(&self.repository)?;
        validate_branch(&self.base_branch)?;
        if self.schema_version != 1 {
            return Err(Error::State("unsupported instance schema".into()));
        }
        match &self.lifecycle {
            Lifecycle::Active { server, .. } if server.id.is_empty() => {
                return Err(Error::State("active server has no ID".into()));
            }
            Lifecycle::Paused { snapshot }
                if snapshot.id.is_empty() || snapshot.source_id.is_empty() =>
            {
                return Err(Error::State(
                    "paused snapshot identity is incomplete".into(),
                ));
            }
            Lifecycle::AllocationPending { correlation, .. } if correlation.is_empty() => {
                return Err(Error::State("allocation correlation is empty".into()));
            }
            _ => {}
        }
        Ok(())
    }
}
/// Rejects instance identifiers that are empty, path-like, oversized, or contain unsafe characters.
pub fn validate_instance_id(v: &str) -> Result<()> {
    if v.is_empty()
        || v.len() > 80
        || !v
            .bytes()
            .enumerate()
            .all(|(i, c)| c.is_ascii_alphanumeric() || i > 0 && matches!(c, b'.' | b'_' | b'-'))
    {
        Err(Error::Cli(
            "instance ID must contain only letters, numbers, dots, underscores, and hyphens".into(),
        ))
    } else {
        Ok(())
    }
}
/// Requires a canonical `owner/repository` name with safe GitHub identifier characters.
pub fn validate_repository(v: &str) -> Result<()> {
    let p: Vec<_> = v.split('/').collect();
    if p.len() != 2
        || p.iter().any(|x| {
            x.is_empty()
                || !x
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b'-'))
        })
    {
        Err(Error::Cli("repository must be OWNER/REPOSITORY".into()))
    } else {
        Ok(())
    }
}
/// Rejects empty, oversized, path-like, or syntactically unsafe Git branch names.
pub fn validate_branch(v: &str) -> Result<()> {
    if v.is_empty()
        || v.starts_with('-')
        || v.contains("..")
        || v.bytes().any(|c| {
            c.is_ascii_control()
                || matches!(c, b' ' | b'~' | b'^' | b':' | b'?' | b'*' | b'[' | b'\\')
        })
    {
        Err(Error::Cli("invalid Git branch".into()))
    } else {
        Ok(())
    }
}
/// Returns the current UTC time in RFC 3339 form for persisted lifecycle records.
pub fn now() -> String {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        .to_string()
}
