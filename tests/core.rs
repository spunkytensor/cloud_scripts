// SPDX-FileCopyrightText: 2026 Matt Curfman
// SPDX-License-Identifier: Apache-2.0

use async_trait::async_trait;
use std::{fs, sync::Mutex};
use tempfile::tempdir;
use vps_control_plane::{
    backend::{Backend, Mutation},
    config::Config,
    error::Result,
    lifecycle::{ControlPlane, DestroyOptions, PauseOptions},
    lifecycle::{pairing_code, parse_pause_marker, remote_usable, validate_snapshot},
    model::*,
    state::{Store, legacy},
};

#[derive(Default)]
struct CountingBackend {
    servers: Mutex<Vec<Server>>,
    server_statuses: Mutex<Vec<String>>,
    actions: Mutex<Vec<Action>>,
    snapshots: Mutex<Vec<Snapshot>>,
    ssh_keys: Mutex<Vec<String>>,
    calls: Mutex<Vec<String>>,
}

struct RejectingBackend {
    state_root: std::path::PathBuf,
    create_calls: Mutex<usize>,
}

#[async_trait]
impl Backend for RejectingBackend {
    /// Simulates successful provider credential validation.
    async fn validate_access(&self) -> Result<()> {
        Ok(())
    }
    /// Confirms allocation intent was persisted before simulating a rejected create request.
    async fn create_server(&self, _: &CreateRecipe, _: Option<&str>) -> Result<Mutation<Server>> {
        let saved = Store::new(self.state_root.clone()).load("x")?;
        assert!(matches!(
            saved.lifecycle,
            Lifecycle::AllocationPending {
                request_intent: true,
                ..
            }
        ));
        *self.create_calls.lock().unwrap() += 1;
        Ok(Mutation::Rejected {
            diagnostic: "invalid recipe".into(),
        })
    }
    /// Simulates a correlation lookup with no matching servers.
    async fn find_servers(&self, _: &str) -> Result<Vec<Server>> {
        Ok(vec![])
    }
    /// Simulates a provider-confirmed missing server.
    async fn get_server(&self, _: &str) -> Result<Option<Server>> {
        Ok(None)
    }
    /// Marks server deletion as unexpected for rejection tests.
    async fn delete_server(&self, _: &str) -> Result<Mutation<()>> {
        unreachable!()
    }
    /// Marks action creation as unexpected for rejection tests.
    async fn action(&self, _: &str, _: &str, _: Option<&str>) -> Result<Mutation<Action>> {
        unreachable!()
    }
    /// Simulates a provider-confirmed missing action.
    async fn get_action(&self, _: &str) -> Result<Option<Action>> {
        Ok(None)
    }
    /// Simulates action correlation with no matches.
    async fn find_actions(&self, _: &str, _: &str, _: &str) -> Result<Vec<Action>> {
        Ok(vec![])
    }
    /// Marks action polling as unexpected for rejection tests.
    async fn wait_action(&self, _: &Action) -> Result<()> {
        unreachable!()
    }
    /// Simulates an empty snapshot inventory.
    async fn snapshots(&self) -> Result<Vec<Snapshot>> {
        Ok(vec![])
    }
    /// Simulates a provider-confirmed missing snapshot.
    async fn get_snapshot(&self, _: &str) -> Result<Option<Snapshot>> {
        Ok(None)
    }
    /// Marks snapshot deletion as unexpected for rejection tests.
    async fn delete_snapshot(&self, _: &str) -> Result<Mutation<()>> {
        unreachable!()
    }
}

#[async_trait]
impl Backend for CountingBackend {
    /// Simulates successful provider credential validation.
    async fn validate_access(&self) -> Result<()> {
        Ok(())
    }
    /// Returns the mock account SSH-key inventory.
    async fn ssh_keys(&self) -> Result<Vec<String>> {
        Ok(self.ssh_keys.lock().unwrap().clone())
    }
    /// Records an unexpected create request before failing the test.
    async fn create_server(&self, _: &CreateRecipe, _: Option<&str>) -> Result<Mutation<Server>> {
        self.calls.lock().unwrap().push("create".into());
        unreachable!()
    }
    /// Records the correlation tag and returns mock servers carrying it.
    async fn find_servers(&self, tag: &str) -> Result<Vec<Server>> {
        self.calls.lock().unwrap().push(format!("find:{tag}"));
        Ok(self
            .servers
            .lock()
            .unwrap()
            .iter()
            .filter(|s| s.tags.iter().any(|t| t == tag))
            .cloned()
            .collect())
    }
    /// Returns a mock server and advances its queued eventually consistent status.
    async fn get_server(&self, id: &str) -> Result<Option<Server>> {
        let mut found = self
            .servers
            .lock()
            .unwrap()
            .iter()
            .find(|s| s.id == id)
            .cloned();
        let mut statuses = self.server_statuses.lock().unwrap();
        if let Some(server) = &mut found
            && !statuses.is_empty()
        {
            server.status = statuses.remove(0);
        }
        Ok(found)
    }
    /// Records and confirms deletion while removing the server from mock inventory.
    async fn delete_server(&self, id: &str) -> Result<Mutation<()>> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("delete-server:{id}"));
        self.servers.lock().unwrap().retain(|s| s.id != id);
        Ok(Mutation::Confirmed(()))
    }
    /// Records an unexpected action request before failing the test.
    async fn action(&self, _: &str, kind: &str, _: Option<&str>) -> Result<Mutation<Action>> {
        self.calls.lock().unwrap().push(format!("action:{kind}"));
        unreachable!()
    }
    /// Returns an action from mock inventory by provider ID.
    async fn get_action(&self, id: &str) -> Result<Option<Action>> {
        Ok(self
            .actions
            .lock()
            .unwrap()
            .iter()
            .find(|a| a.id == id)
            .cloned())
    }
    /// Records correlation and returns actions matching resource and kind.
    async fn find_actions(&self, resource: &str, kind: &str, _: &str) -> Result<Vec<Action>> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("find-action:{kind}"));
        Ok(self
            .actions
            .lock()
            .unwrap()
            .iter()
            .filter(|a| a.resource_id == resource && a.kind == kind)
            .cloned()
            .collect())
    }
    /// Simulates an action that has already completed.
    async fn wait_action(&self, _: &Action) -> Result<()> {
        Ok(())
    }
    /// Returns the complete mock snapshot inventory.
    async fn snapshots(&self) -> Result<Vec<Snapshot>> {
        Ok(self.snapshots.lock().unwrap().clone())
    }
    /// Returns a snapshot from mock inventory by provider ID.
    async fn get_snapshot(&self, id: &str) -> Result<Option<Snapshot>> {
        Ok(self
            .snapshots
            .lock()
            .unwrap()
            .iter()
            .find(|s| s.id == id)
            .cloned())
    }
    /// Records and confirms deletion while removing the snapshot from mock inventory.
    async fn delete_snapshot(&self, id: &str) -> Result<Mutation<()>> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("delete-snapshot:{id}"));
        self.snapshots.lock().unwrap().retain(|s| s.id != id);
        Ok(Mutation::Confirmed(()))
    }
}
/// Builds the minimal deterministic allocation recipe shared by lifecycle tests.
fn recipe() -> CreateRecipe {
    CreateRecipe {
        name: "n".into(),
        region: "r".into(),
        size: "s".into(),
        image: "i".into(),
        tags: vec![],
        ssh_key: "k".into(),
    }
}
/// Builds an offline mock server with the supplied provider ID.
fn server(id: &str) -> Server {
    Server {
        id: id.into(),
        name: "n".into(),
        endpoint: Some("192.0.2.1".into()),
        region: "r".into(),
        size: "s".into(),
        image: "i".into(),
        tags: vec![],
        disk_gb: 25,
        status: "off".into(),
        volume_ids: vec![],
    }
}

/// Configuration expands a tilde-prefixed SSH private-key path against the user home.
#[cfg(unix)]
#[test]
fn config_expands_tilde_in_ssh_private_key() {
    let d = tempdir().unwrap();
    let path = d.path().join("vps.toml");
    fs::write(
        &path,
        "version = 1\n[ssh]\nprivate_key = \"~/.ssh/id_digitalocean\"\n",
    )
    .unwrap();

    let (config, _) = Config::load(Some(&path), Some(d.path().join("home"))).unwrap();

    assert_eq!(
        config.ssh.private_key.unwrap(),
        std::path::PathBuf::from(std::env::var_os("HOME").unwrap()).join(".ssh/id_digitalocean")
    );
}

/// Builds a valid test instance in the supplied lifecycle state.
fn instance(lifecycle: Lifecycle) -> Instance {
    Instance {
        schema_version: 1,
        instance_id: "x".into(),
        backend: "digitalocean".into(),
        created_at: now(),
        repository: "o/r".into(),
        base_branch: "main".into(),
        work_branch: "codex/x".into(),
        lifecycle,
        provider: serde_json::json!({}),
    }
}

/// Rerunning an allocation with persisted request intent reconciles instead of creating again.
#[tokio::test]
async fn allocation_rerun_never_calls_create() {
    let d = tempdir().unwrap();
    let store = Store::new(d.path().into());
    store
        .save(&instance(Lifecycle::AllocationPending {
            recipe: recipe(),
            correlation: "corr".into(),
            request_intent: true,
        }))
        .unwrap();
    let backend = CountingBackend::default();
    let cfg = Config::default();
    let cp = ControlPlane {
        store: &store,
        config: &cfg,
        backend: &backend,
    };
    assert!(
        cp.create("digitalocean", "x".into(), "o/r".into(), "main".into())
            .await
            .is_err()
    );
    assert!(!backend.calls.lock().unwrap().iter().any(|c| c == "create"));
}

/// A rejected create clears request intent but preserves its recipe and correlation for an exact retry.
#[tokio::test]
async fn rejected_allocation_is_durably_intended_then_retryable_with_same_recipe() {
    let d = tempdir().unwrap();
    let store = Store::new(d.path().into());
    let original = recipe();
    store
        .save(&instance(Lifecycle::AllocationPending {
            recipe: original.clone(),
            correlation: "corr".into(),
            request_intent: false,
        }))
        .unwrap();
    fs::write(d.path().join("key.pub"), "ssh-ed25519 AAAA test-key\n").unwrap();
    let backend = RejectingBackend {
        state_root: d.path().into(),
        create_calls: Mutex::new(0),
    };
    let mut cfg = Config::default();
    cfg.ssh.private_key = Some(d.path().join("key"));
    let cp = ControlPlane {
        store: &store,
        config: &cfg,
        backend: &backend,
    };
    for expected_calls in 1..=2 {
        assert!(
            cp.create("digitalocean", "x".into(), "o/r".into(), "main".into())
                .await
                .is_err()
        );
        assert_eq!(*backend.create_calls.lock().unwrap(), expected_calls);
        let saved = store.load("x").unwrap();
        let Lifecycle::AllocationPending {
            recipe,
            correlation,
            request_intent,
        } = saved.lifecycle
        else {
            panic!("rejected allocation recipe was lost")
        };
        assert!(!request_intent);
        assert_eq!(correlation, "corr");
        assert_eq!(recipe.name, original.name);
        assert_eq!(recipe.tags, original.tags);
    }
}

/// Creating from source-reserved state persists a fresh correlated allocation before provisioning.
#[tokio::test]
async fn source_reserved_create_is_persisted_as_fresh_allocation_not_success() {
    let d = tempdir().unwrap();
    let store = Store::new(d.path().into());
    store
        .save(&instance(Lifecycle::SourceReserved { recipe: recipe() }))
        .unwrap();
    let backend = CountingBackend::default();
    backend.ssh_keys.lock().unwrap().push("only-key".into());
    let mut cfg = Config::default();
    cfg.ssh.private_key = Some(d.path().join("missing-key"));
    let result = ControlPlane {
        store: &store,
        config: &cfg,
        backend: &backend,
    }
    .create("digitalocean", "x".into(), "o/r".into(), "main".into())
    .await;
    assert!(result.is_err());
    let saved = store.load("x").unwrap();
    let Lifecycle::AllocationPending {
        recipe,
        correlation,
        request_intent,
    } = saved.lifecycle
    else {
        panic!("source reservation was not converted")
    };
    assert_eq!(recipe.ssh_key, "only-key");
    assert!(recipe.tags.contains(&correlation));
    assert!(!request_intent);
}

/// Destroying source-reserved state removes only local state and never contacts the provider.
#[tokio::test]
async fn source_reserved_destroy_never_queries_or_deletes_provider() {
    let d = tempdir().unwrap();
    let store = Store::new(d.path().into());
    store
        .save(&instance(Lifecycle::SourceReserved { recipe: recipe() }))
        .unwrap();
    let backend = CountingBackend::default();
    ControlPlane {
        store: &store,
        config: &Config::default(),
        backend: &backend,
    }
    .destroy("x", DestroyOptions::default())
    .await
    .unwrap();
    assert!(backend.calls.lock().unwrap().is_empty());
    assert!(!store.dir("x").exists());
}

/// Create rejects an existing instance owned by another backend before provider access.
#[tokio::test]
async fn create_rejects_existing_instance_from_another_backend() {
    let d = tempdir().unwrap();
    let store = Store::new(d.path().into());
    let mut existing = instance(Lifecycle::SourceReserved { recipe: recipe() });
    existing.backend = "other-cloud".into();
    store.save(&existing).unwrap();
    let backend = CountingBackend::default();

    let result = ControlPlane {
        store: &store,
        config: &Config::default(),
        backend: &backend,
    }
    .create("digitalocean", "x".into(), "o/r".into(), "main".into())
    .await;

    assert!(result.is_err());
    assert!(backend.calls.lock().unwrap().is_empty());
}

/// Snapshot validation accepts any advertised source region and rejects a minimum disk larger than the source.
#[test]
fn snapshot_validation_uses_region_membership_and_source_disk_direction() {
    let mut snap = Snapshot {
        id: "9".into(),
        name: "snap".into(),
        source_id: "1".into(),
        region: "first".into(),
        regions: vec!["first".into(), "source".into()],
        min_disk_gb: 24,
        host_key: String::new(),
        pause_operation_id: String::new(),
        source_recipe: None,
    };
    assert!(validate_snapshot(&snap, "9", "snap", "1", "source", 25).is_ok());
    assert!(validate_snapshot(&snap, "9", "snap", "1", "absent", 25).is_err());
    snap.min_disk_gb = 26;
    assert!(validate_snapshot(&snap, "9", "snap", "1", "source", 25).is_err());
}

/// Completed pause and resume journals remain valid until the next lifecycle call clears them.
#[tokio::test]
async fn completed_transitions_remain_readable_until_journal_cleanup() {
    let d = tempdir().unwrap();
    let store = Store::new(d.path().into());
    let snap = Snapshot {
        id: "9".into(),
        name: "snap".into(),
        source_id: "1".into(),
        region: "r".into(),
        min_disk_gb: 25,
        host_key: "ssh-ed25519 key".into(),
        pause_operation_id: "op".into(),
        source_recipe: Some(recipe()),
        regions: vec!["r".into()],
    };
    let mut checkpoints = Checkpoints {
        quiescence_verified: true,
        captured_host_key: Some("ssh-ed25519 key".into()),
        source_delete_intent: true,
        source_delete_confirmed: true,
        ..Default::default()
    };
    let transition = Transition {
        schema_version: 1,
        kind: TransitionKind::Pause,
        phase: Phase::PausingDeletePending,
        operation_id: "op".into(),
        started_at: now(),
        checkpoints: checkpoints.clone(),
        source: Some(server("1")),
        snapshot: Some(snap.clone()),
        target_recipe: None,
        target: None,
        correlation: None,
    };
    store
        .save(&instance(Lifecycle::Active {
            server: server("1"),
            snapshot: None,
        }))
        .unwrap();
    store.save_transition("x", &transition).unwrap();
    store
        .save(&instance(Lifecycle::Paused {
            snapshot: snap.clone(),
        }))
        .unwrap();
    assert!(store.transition("x").unwrap().is_some());

    checkpoints.snapshot_delete_confirmed = true;
    let resume = Transition {
        schema_version: 1,
        kind: TransitionKind::Resume,
        phase: Phase::ActiveSnapshotCleanupPending,
        operation_id: "resume-op".into(),
        started_at: now(),
        checkpoints,
        source: None,
        snapshot: Some(snap),
        target_recipe: Some(recipe()),
        target: Some(server("2")),
        correlation: Some("resume-correlation".into()),
    };
    store
        .save(&instance(Lifecycle::Active {
            server: server("2"),
            snapshot: Some(resume.snapshot.clone().unwrap()),
        }))
        .unwrap();
    store.save_transition("x", &resume).unwrap();
    store
        .save(&instance(Lifecycle::Active {
            server: server("2"),
            snapshot: None,
        }))
        .unwrap();
    assert!(store.transition("x").unwrap().is_some());

    let backend = CountingBackend::default();
    ControlPlane {
        store: &store,
        config: &Config::default(),
        backend: &backend,
    }
    .resume("x", Default::default())
    .await
    .unwrap();
    assert!(store.transition("x").unwrap().is_none());
    assert!(backend.calls.lock().unwrap().is_empty());
}

/// A reconciled shutdown action still waits until eventually consistent server status becomes `off`.
#[tokio::test]
async fn persisted_shutdown_waits_for_eventually_consistent_server_status() {
    let d = tempdir().unwrap();
    let store = Store::new(d.path().into());
    let s = server("1");
    store
        .save(&instance(Lifecycle::Active {
            server: s.clone(),
            snapshot: None,
        }))
        .unwrap();
    let snap = Snapshot {
        id: "9".into(),
        name: "vps-x-op".into(),
        source_id: "1".into(),
        region: "r".into(),
        min_disk_gb: 25,
        host_key: "".into(),
        pause_operation_id: "".into(),
        source_recipe: None,
        regions: vec!["r".into()],
    };
    let checkpoints = Checkpoints {
        quiescence_verified: true,
        captured_host_key: Some("ssh-ed25519 test-key".into()),
        shutdown_intent: true,
        snapshot_intent: true,
        ..Default::default()
    };
    store
        .save_transition(
            "x",
            &Transition {
                schema_version: 1,
                kind: TransitionKind::Pause,
                phase: Phase::PausingShutdown,
                operation_id: "op".into(),
                started_at: "2026-01-01".into(),
                checkpoints,
                source: Some(s.clone()),
                snapshot: None,
                target_recipe: None,
                target: None,
                correlation: None,
            },
        )
        .unwrap();
    let backend = CountingBackend::default();
    *backend.servers.lock().unwrap() = vec![s];
    *backend.server_statuses.lock().unwrap() = vec!["active".into(), "active".into(), "off".into()];
    *backend.actions.lock().unwrap() = vec![
        Action {
            id: "a".into(),
            kind: "shutdown".into(),
            resource_id: "1".into(),
            status: ActionStatus::Completed,
            started_at: Some("2026-01-02".into()),
        },
        Action {
            id: "b".into(),
            kind: "snapshot".into(),
            resource_id: "1".into(),
            status: ActionStatus::Completed,
            started_at: Some("2026-01-02".into()),
        },
    ];
    *backend.snapshots.lock().unwrap() = vec![snap];
    let mut cfg = Config::default();
    cfg.ssh.private_key = Some(d.path().join("unused"));
    cfg.backends.digitalocean.ssh_key = Some("k".into());
    ControlPlane {
        store: &store,
        config: &cfg,
        backend: &backend,
    }
    .pause("x", PauseOptions::default())
    .await
    .unwrap();
    assert!(
        !backend
            .calls
            .lock()
            .unwrap()
            .iter()
            .any(|c| c.starts_with("action:"))
    );
    assert!(backend.server_statuses.lock().unwrap().is_empty());
}

/// Destroy confirms deletion of every frozen server before deleting retained snapshots.
#[tokio::test]
async fn destroy_deletes_every_server_before_snapshots() {
    let d = tempdir().unwrap();
    let store = Store::new(d.path().into());
    let a = server("1");
    let b = server("2");
    let c = server("3");
    let snap = Snapshot {
        id: "9".into(),
        name: "snap".into(),
        source_id: "1".into(),
        region: "r".into(),
        min_disk_gb: 25,
        host_key: "k".into(),
        pause_operation_id: "p".into(),
        source_recipe: None,
        regions: vec!["r".into()],
    };
    store
        .save(&instance(Lifecycle::Active {
            server: a.clone(),
            snapshot: Some(snap.clone()),
        }))
        .unwrap();
    store
        .save_transition(
            "x",
            &Transition {
                schema_version: 1,
                kind: TransitionKind::Resume,
                phase: Phase::ActiveSnapshotCleanupPending,
                operation_id: "op".into(),
                started_at: now(),
                checkpoints: Checkpoints {
                    recovery_verified_at: Some(now()),
                    teardown_servers: vec![a.clone(), b.clone(), c.clone()],
                    ..Default::default()
                },
                source: None,
                snapshot: Some(snap.clone()),
                target_recipe: Some(recipe()),
                target: Some(b.clone()),
                correlation: Some("c".into()),
            },
        )
        .unwrap();
    let backend = CountingBackend::default();
    *backend.servers.lock().unwrap() = vec![a, b, c];
    *backend.snapshots.lock().unwrap() = vec![snap];
    ControlPlane {
        store: &store,
        config: &Config::default(),
        backend: &backend,
    }
    .destroy("x", DestroyOptions::default())
    .await
    .unwrap();
    let calls = backend.calls.lock().unwrap();
    let snapshot = calls
        .iter()
        .position(|c| c.starts_with("delete-snapshot"))
        .unwrap();
    assert_eq!(
        calls[..snapshot]
            .iter()
            .filter(|c| c.starts_with("delete-server"))
            .count(),
        3
    );
}

/// Retrying a started destroy uses its frozen server inventory without another correlation lookup.
#[tokio::test]
async fn destroy_retry_uses_frozen_inventory_without_reresolving_correlations() {
    let d = tempdir().unwrap();
    let store = Store::new(d.path().into());
    let allocated = server("1");
    store
        .save(&instance(Lifecycle::AllocationPending {
            recipe: recipe(),
            correlation: "create-correlation".into(),
            request_intent: true,
        }))
        .unwrap();
    store
        .save_transition(
            "x",
            &Transition {
                schema_version: 1,
                kind: TransitionKind::Destroy,
                phase: Phase::Destroying,
                operation_id: "destroy-op".into(),
                started_at: now(),
                checkpoints: Checkpoints {
                    teardown_started: true,
                    teardown_servers: vec![allocated.clone()],
                    ..Default::default()
                },
                source: Some(allocated),
                snapshot: None,
                target_recipe: None,
                target: None,
                correlation: Some("create-correlation".into()),
            },
        )
        .unwrap();
    let backend = CountingBackend::default();

    ControlPlane {
        store: &store,
        config: &Config::default(),
        backend: &backend,
    }
    .destroy(
        "x",
        DestroyOptions {
            confirm_missing_server: true,
            ..Default::default()
        },
    )
    .await
    .unwrap();

    assert!(!store.dir("x").exists());
    assert!(
        !backend
            .calls
            .lock()
            .unwrap()
            .iter()
            .any(|call| call.starts_with("find:"))
    );
}

/// Adopted legacy teardown markers without frozen inventory reconcile active and correlated servers.
#[tokio::test]
async fn legacy_destroy_reconciles_inventory_before_freezing_it() {
    let d = tempdir().unwrap();
    let dir = d.path().join("x");
    fs::create_dir(&dir).unwrap();
    legacy_fixture(&dir, "resuming-recovery", "resume");
    fs::remove_file(dir.join("current.env.paused")).unwrap();
    fs::write(
        dir.join("current.env"),
        "DROPLET_ID=101\nDROPLET_IP=192.0.2.1\nDROPLET_NAME=worker\nKNOWN_HOSTS_FILE=x\nSSH_CONFIG_FILE=x\nSSH_ALIAS=x\nCREATED_AT=now\n",
    )
    .unwrap();
    let transition = fs::read_to_string(dir.join("current.env.transition"))
        .unwrap()
        .replace("TARGET_DROPLET_ID=202", "TARGET_DROPLET_ID=")
        .replace("TARGET_DROPLET_IP=192.0.2.2", "TARGET_DROPLET_IP=")
        .replace("TEARDOWN_STARTED=0", "TEARDOWN_STARTED=1");
    fs::write(dir.join("current.env.transition"), transition).unwrap();

    let active = server("101");
    let mut correlated = server("202");
    correlated.tags.push("resume-tag".into());
    let snapshot = Snapshot {
        id: "9".into(),
        name: "snap".into(),
        source_id: "101".into(),
        region: "nyc3".into(),
        min_disk_gb: 25,
        host_key: "ssh-ed25519 AAAA".into(),
        pause_operation_id: "pause-1".into(),
        source_recipe: None,
        regions: vec!["nyc3".into()],
    };
    let backend = CountingBackend::default();
    *backend.servers.lock().unwrap() = vec![active, correlated];
    *backend.snapshots.lock().unwrap() = vec![snapshot];
    let store = Store::new(d.path().into());

    ControlPlane {
        store: &store,
        config: &Config::default(),
        backend: &backend,
    }
    .destroy("x", DestroyOptions::default())
    .await
    .unwrap();

    assert!(!store.dir("x").exists());
    assert!(backend.servers.lock().unwrap().is_empty());
    assert!(backend.snapshots.lock().unwrap().is_empty());
    let calls = backend.calls.lock().unwrap();
    assert!(calls.iter().any(|call| call == "find:resume-tag"));
    assert!(calls.iter().any(|call| call == "delete-server:101"));
    assert!(calls.iter().any(|call| call == "delete-server:202"));
    assert!(calls.iter().any(|call| call == "delete-snapshot:9"));
}

/// Remote-control status and pairing extraction accept only explicit supported JSON fields and values.
#[test]
fn remote_control_json_decisions_are_strict() {
    assert!(remote_usable(&serde_json::json!({"status":"connected"})));
    assert!(remote_usable(&serde_json::json!({"state":"connecting"})));
    assert!(!remote_usable(&serde_json::json!({"status":"stopped"})));
    assert_eq!(
        pairing_code(&serde_json::json!({"pairing_code":"ABCD"})),
        Some("ABCD")
    );
    assert_eq!(pairing_code(&serde_json::json!({"code":""})), None);
}

/// Pause markers reject unknown fields and container IDs outside the exact 64-hex format.
#[test]
fn pause_marker_parser_requires_exact_fields_and_container_ids() {
    let id = "a".repeat(64);
    let marker = format!(
        "OPERATION_ID=pause-1\nSSH_HOST_ED25519_PUBLIC_KEY=ssh-ed25519\\ AAAA\nRUNNING_CONTAINER_IDS={id}\nHEALTHY_CONTAINER_IDS=\n"
    );
    let parsed = parse_pause_marker(&marker).unwrap();
    assert_eq!(parsed.operation_id, "pause-1");
    assert_eq!(parsed.running_container_ids, vec![id]);
    assert!(parse_pause_marker(&(marker.clone() + "EXTRA=x\n")).is_err());
    assert!(parse_pause_marker(&marker.replace(&"a".repeat(64), "not-an-id")).is_err());
}

/// Legacy state parsing decodes escaped spaces but rejects command substitution as inert data.
#[test]
fn legacy_parser_is_non_executable() {
    assert!(legacy::parse("A=$(touch /tmp/nope)\n").is_err());
    assert_eq!(
        legacy::parse("A=hello\\ world\n").unwrap()["A"],
        "hello world"
    )
}
/// Native state round-trips, excludes incomplete directories, and uses owner-only files on Unix.
#[test]
fn state_round_trip_and_permissions() {
    let d = tempdir().unwrap();
    let s = Store::new(d.path().into());
    let i = Instance {
        schema_version: 1,
        instance_id: "test-a".into(),
        backend: "digitalocean".into(),
        created_at: now(),
        repository: "o/r".into(),
        base_branch: "main".into(),
        work_branch: "codex/test-a".into(),
        lifecycle: Lifecycle::SourceReserved {
            recipe: CreateRecipe {
                name: "n".into(),
                region: "r".into(),
                size: "s".into(),
                image: "i".into(),
                tags: vec![],
                ssh_key: "k".into(),
            },
        },
        provider: serde_json::json!({"x":1}),
    };
    s.save(&i).unwrap();
    s.save_secret("test-a", "github-token", b"github_pat_test")
        .unwrap();
    fs::create_dir(s.dir("incomplete-command")).unwrap();
    fs::write(s.dir("incomplete-command").join("lifecycle.lock"), b"").unwrap();
    assert!(s.load_or_adopt("typo").is_err());
    assert!(!s.dir("typo").exists());
    assert_eq!(s.load("test-a").unwrap().repository, "o/r");
    assert_eq!(s.ids().unwrap(), vec!["test-a"]);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(s.dir("test-a").join("instance.json"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(s.dir("test-a").join("github-token"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
    #[cfg(windows)]
    {
        assert!(s.dir("test-a").join("instance.json").is_file());
        assert!(s.dir("test-a").join("github-token").is_file());
    }
}
/// Identifier validation rejects traversal and malformed Git references while accepting a normal repository.
#[test]
fn validation_rejects_paths() {
    assert!(validate_instance_id("../x").is_err());
    assert!(validate_repository("owner/repo").is_ok());
    assert!(validate_branch("bad..branch").is_err())
}

/// Writes a complete legacy fixture for one pause or resume transition phase.
fn legacy_fixture(dir: &std::path::Path, phase: &str, kind: &str) {
    fs::write(
        dir.join("current.env.setup"),
        "SETUP_REPOSITORY=o/r\nSETUP_BASE_BRANCH=main\nSETUP_WORK_BRANCH=codex/x\n",
    )
    .unwrap();
    if kind == "pause" || phase == "active-snapshot-cleanup-pending" {
        fs::write(dir.join("current.env"), "DROPLET_ID=101\nDROPLET_IP=192.0.2.1\nDROPLET_NAME=worker\nKNOWN_HOSTS_FILE=x\nSSH_CONFIG_FILE=x\nSSH_ALIAS=x\nCREATED_AT=now\n").unwrap();
    } else {
        fs::write(dir.join("current.env.paused"), "PAUSED_STATE_VERSION=1\nPAUSE_OPERATION_ID=pause-1\nSNAPSHOT_ID=9\nSNAPSHOT_NAME=snap\nSNAPSHOT_SOURCE_DROPLET_ID=101\nSNAPSHOT_CREATED_AT=now\nDROPLET_NAME=worker\nDROPLET_REGION=nyc3\nDROPLET_SIZE=s-1\nDROPLET_TAGS=a,b\nSOURCE_DISK_SIZE=25\nSSH_HOST_ED25519_PUBLIC_KEY=ssh-ed25519\\ AAAA\nPAUSED_AT=now\n").unwrap();
    }
    let transition = format!(
        "TRANSITION_VERSION=1\nTRANSITION_KIND={kind}\nTRANSITION_PHASE={phase}\nOPERATION_ID=op\nPAUSE_OPERATION_ID=pause-1\nTRANSITION_STARTED_AT=now\nSOURCE_DROPLET_ID=101\nSOURCE_DROPLET_NAME=worker\nSOURCE_DROPLET_IP=192.0.2.1\nSOURCE_REGION=nyc3\nSOURCE_SIZE=s-1\nSOURCE_TAGS=a,b\nSOURCE_DISK_SIZE=25\nREMOTE_QUIESCE_VERIFIED=1\nSHUTDOWN_REQUESTED=1\nSHUTDOWN_ACTION_ID=2\nPOWER_OFF_REQUESTED=0\nPOWER_OFF_ACTION_ID=\nSNAPSHOT_NAME=snap\nSNAPSHOT_REQUESTED=1\nSNAPSHOT_ACTION_ID=3\nSNAPSHOT_ID=9\nSNAPSHOT_CREATED_AT=now\nSOURCE_DELETE_REQUESTED=1\nSOURCE_DELETE_CONFIRMED=0\nSSH_HOST_ED25519_PUBLIC_KEY=ssh-ed25519\\ AAAA\nTARGET_DROPLET_NAME=worker\nTARGET_REGION=nyc3\nTARGET_SIZE=s-1\nTARGET_TAGS=a,b\nTARGET_LIFECYCLE_TAG=resume-tag\nTARGET_CREATE_REQUESTED=1\nTARGET_DROPLET_ID=202\nTARGET_DROPLET_IP=192.0.2.2\nRECOVERY_VERIFIED_AT=\nSNAPSHOT_DELETE_REQUESTED=0\nSNAPSHOT_DELETE_CONFIRMED=0\nTEARDOWN_STARTED=0\nTARGET_DELETE_CONFIRMED=0\n"
    );
    fs::write(dir.join("current.env.transition"), transition).unwrap();
}

/// Adoption converts every legacy transition phase and exposes its native journal immediately.
#[test]
fn adopts_all_seven_legacy_phases_and_mutating_view_sees_transition() {
    for (phase, kind) in [
        ("pausing-quiescing", "pause"),
        ("pausing-shutdown", "pause"),
        ("pausing-snapshot", "pause"),
        ("pausing-delete-pending", "pause"),
        ("resuming-allocation", "resume"),
        ("resuming-recovery", "resume"),
        ("active-snapshot-cleanup-pending", "resume"),
    ] {
        let d = tempdir().unwrap();
        let instance = d.path().join("x");
        fs::create_dir(&instance).unwrap();
        legacy_fixture(&instance, phase, kind);
        let store = Store::new(d.path().into());
        let lock = store.lock("x").unwrap();
        let adopted = store.load_or_adopt_locked("x").unwrap();
        drop(lock);
        assert_eq!(adopted.instance_id, "x");
        assert!(store.transition("x").unwrap().is_some());
        assert!(instance.join("current.env.transition").exists());
        assert!(instance.join("migration-complete.json").exists());
    }
}

/// Missing legacy pause identifiers produce a state error rather than panicking during import.
#[test]
fn malformed_legacy_snapshot_transition_returns_state_error_without_panicking() {
    let d = tempdir().unwrap();
    legacy_fixture(d.path(), "pausing-snapshot", "pause");
    let transition = fs::read_to_string(d.path().join("current.env.transition"))
        .unwrap()
        .lines()
        .filter(|line| {
            !line.starts_with("OPERATION_ID=") && !line.starts_with("PAUSE_OPERATION_ID=")
        })
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(
        d.path().join("current.env.transition"),
        format!("{transition}\n"),
    )
    .unwrap();

    let result = std::panic::catch_unwind(|| legacy::import(d.path(), "x"));
    assert!(result.is_ok(), "legacy import panicked");
    assert!(result.unwrap().is_err());
}

/// Interrupted adoption replays legacy authority, replacing an obsolete partial native journal.
#[test]
fn incomplete_legacy_adoption_replays_and_removes_obsolete_transition() {
    let d = tempdir().unwrap();
    let dir = d.path().join("x");
    fs::create_dir(&dir).unwrap();
    fs::write(
        dir.join("current.env.setup"),
        "SETUP_REPOSITORY=o/r\nSETUP_BASE_BRANCH=main\nSETUP_WORK_BRANCH=codex/x\n",
    )
    .unwrap();
    let store = Store::new(d.path().into());
    store
        .save(&instance(Lifecycle::AllocationPending {
            recipe: recipe(),
            correlation: "partial-native".into(),
            request_intent: false,
        }))
        .unwrap();
    fs::write(dir.join("transition.json"), b"obsolete and invalid").unwrap();
    let adopted = store.load_or_adopt("x").unwrap();
    assert!(matches!(
        adopted.lifecycle,
        Lifecycle::SourceReserved { .. }
    ));
    assert!(!dir.join("transition.json").exists());
    assert!(dir.join("migration-complete.json").exists());
    assert!(dir.join("current.env.setup").exists());
}

/// Legacy allocation intent maps only `0` and `1`; other boolean spellings are rejected.
#[test]
fn legacy_allocation_requested_maps_strictly() {
    for (value, expected) in [("0", false), ("1", true)] {
        let d = tempdir().unwrap();
        fs::write(
            d.path().join("current.env.setup"),
            "SETUP_REPOSITORY=o/r\nSETUP_BASE_BRANCH=main\n",
        )
        .unwrap();
        fs::write(
            d.path().join("current.env.allocation"),
            format!("ALLOCATION_DROPLET_NAME=n\nALLOCATION_REGION=r\nALLOCATION_SIZE=s\nALLOCATION_IMAGE=i\nALLOCATION_TAGS=corr\nALLOCATION_LIFECYCLE_TAG=corr\nALLOCATION_REQUESTED={value}\n"),
        )
        .unwrap();
        let imported = legacy::import(d.path(), "x").unwrap();
        assert!(matches!(
            imported.instance.lifecycle,
            Lifecycle::AllocationPending {
                request_intent,
                ..
            } if request_intent == expected
        ));
    }

    let d = tempdir().unwrap();
    fs::write(
        d.path().join("current.env.setup"),
        "SETUP_REPOSITORY=o/r\nSETUP_BASE_BRANCH=main\n",
    )
    .unwrap();
    fs::write(
        d.path().join("current.env.allocation"),
        "ALLOCATION_DROPLET_NAME=n\nALLOCATION_REGION=r\nALLOCATION_SIZE=s\nALLOCATION_IMAGE=i\nALLOCATION_TAGS=corr\nALLOCATION_LIFECYCLE_TAG=corr\nALLOCATION_REQUESTED=yes\n",
    )
    .unwrap();
    assert!(legacy::import(d.path(), "x").is_err());
}

/// Legacy import rejects executable syntax and simultaneous active and paused state evidence.
#[test]
fn legacy_import_rejects_contradictory_and_malformed_state() {
    let d = tempdir().unwrap();
    fs::write(
        d.path().join("current.env.setup"),
        "SETUP_REPOSITORY=o/r\nSETUP_BASE_BRANCH=main\n",
    )
    .unwrap();
    fs::write(
        d.path().join("current.env"),
        "BROKEN=$(touch /tmp/not-run)\n",
    )
    .unwrap();
    assert!(legacy::import(d.path(), "x").is_err());
    fs::write(d.path().join("current.env"),"DROPLET_ID=1\nDROPLET_NAME=n\nDROPLET_IP=\nKNOWN_HOSTS_FILE=x\nSSH_CONFIG_FILE=x\nSSH_ALIAS=x\nCREATED_AT=x\n").unwrap();
    fs::write(
        d.path().join("current.env.paused"),
        "PAUSED_STATE_VERSION=1\n",
    )
    .unwrap();
    assert!(legacy::import(d.path(), "x").is_err());
}
