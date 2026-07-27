// SPDX-FileCopyrightText: 2026 Matt Curfman
// SPDX-License-Identifier: Apache-2.0

pub mod legacy;
use crate::{
    error::{Error, Result},
    model::{Instance, Transition},
};
use fs2::FileExt;
use serde::Serialize;
use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};
use uuid::Uuid;

#[derive(Clone)]
pub struct Store {
    pub root: PathBuf,
}
pub struct Lock {
    _file: File,
}
impl Store {
    /// Creates a store rooted at the directory containing per-instance state.
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }
    /// Returns the state directory for a validated instance identifier.
    pub fn dir(&self, id: &str) -> PathBuf {
        self.root.join(id)
    }
    /// Creates the instance directory if necessary and acquires its exclusive lifecycle lock.
    pub fn lock(&self, id: &str) -> Result<Lock> {
        let d = self.dir(id);
        fs::create_dir_all(&d)?;
        self.lock_dir(id, &d)
    }
    /// Acquires an exclusive lock only for an already existing instance directory.
    pub fn lock_existing(&self, id: &str) -> Result<Lock> {
        let d = self.dir(id);
        if !d.is_dir() {
            return Err(Error::State(format!("instance {id} does not exist")));
        }
        self.lock_dir(id, &d)
    }
    /// Secures `d` and acquires its nonblocking exclusive lifecycle lock.
    fn lock_dir(&self, id: &str, d: &Path) -> Result<Lock> {
        secure_dir(d)?;
        let f = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(d.join("lifecycle.lock"))?;
        f.try_lock_exclusive().map_err(|_| {
            Error::State(format!(
                "instance {id} is locked by another lifecycle command"
            ))
        })?;
        Ok(Lock { _file: f })
    }
    /// Reads native instance JSON and validates every persisted lifecycle invariant.
    pub fn load(&self, id: &str) -> Result<Instance> {
        let i: Instance = serde_json::from_slice(&fs::read(self.dir(id).join("instance.json"))?)?;
        i.validate()?;
        Ok(i)
    }
    /// Load native state, or adopt legacy state while the caller holds the instance lock.
    pub fn load_or_adopt_locked(&self, id: &str) -> Result<Instance> {
        let dir = self.dir(id);
        // The marker, not instance.json, commits adoption.  An interrupted import is
        // deliberately replayed from the retained legacy authority.
        if dir.join("current.env.setup").is_file() && !dir.join("migration-complete.json").is_file()
        {
            return self.import_legacy(id);
        }
        match self.load(id) {
            Ok(i) => Ok(i),
            Err(e) => Err(e),
        }
    }
    /// Converts retained legacy files into durable native state and commits a migration marker last.
    /// Interrupted imports remain replayable because legacy evidence is never removed.
    fn import_legacy(&self, id: &str) -> Result<Instance> {
        let imported = legacy::import(&self.dir(id), id)?;
        self.save(&imported.instance)?;
        if let Some(t) = &imported.transition {
            self.save_transition(id, t)?;
        } else {
            self.clear_transition(id)?;
        }
        for (legacy_name, native_name) in [
            ("current.env.github-token", "github-token"),
            (
                "current.env.github-token.replacement",
                "github-token.replacement",
            ),
            ("current.env.known_hosts", "known_hosts"),
            ("current.env.ssh_config", "ssh_config"),
            ("current.env.remote-control", "remote-control.legacy"),
            ("current.env.chatgpt-login", "chatgpt-login.legacy"),
        ] {
            let source = self.dir(id).join(legacy_name);
            if source.is_file() {
                if native_name.starts_with("github-token") && !imported.credentials_owned {
                    return Err(Error::State(
                        "legacy credentials are not instance-owned".into(),
                    ));
                }
                self.save_secret(id, native_name, &fs::read(source)?)?;
                // Every expected native artifact must be readable before commit.
                fs::read(self.dir(id).join(native_name))?;
            }
        }
        let reread = self.load(id)?;
        // Re-read transition too: a mutating caller must see adopted intent immediately.
        if imported.transition.is_some() {
            self.transition(id)?
                .ok_or_else(|| Error::State("adopted transition disappeared".into()))?;
        }
        atomic_json(
            &self.dir(id).join("migration-complete.json"),
            &serde_json::json!({"schema_version":1,"legacy_files_retained":true}),
        )?;
        Ok(reread)
    }
    /// Locks an existing instance, then loads native state or completes legacy adoption.
    pub fn load_or_adopt(&self, id: &str) -> Result<Instance> {
        let _lock = self.lock_existing(id)?;
        self.load_or_adopt_locked(id)
    }
    /// Loads and validates the optional transition journal against current instance state.
    pub fn transition(&self, id: &str) -> Result<Option<Transition>> {
        let p = self.dir(id).join("transition.json");
        if !p.exists() {
            return Ok(None);
        }
        let t: Transition = serde_json::from_slice(&fs::read(p)?)?;
        let i = self.load(id)?;
        t.validate(&i)?;
        Ok(Some(t))
    }
    /// Validates and atomically persists the instance state.
    pub fn save(&self, i: &Instance) -> Result<()> {
        i.validate()?;
        atomic_json(&self.dir(&i.instance_id).join("instance.json"), i)
    }
    /// Atomically persists secret in the private instance state directory.
    pub fn save_secret(&self, id: &str, name: &str, value: &[u8]) -> Result<()> {
        if name.contains('/') || name.starts_with('.') {
            return Err(Error::State("invalid secret state name".into()));
        }
        atomic_bytes(&self.dir(id).join(name), value)
    }
    /// Removes persisted secret, treating an absent file as already complete.
    pub fn remove_secret(&self, id: &str, name: &str) -> Result<()> {
        if name.contains('/') || name.starts_with('.') {
            return Err(Error::State("invalid secret state name".into()));
        }
        let path = self.dir(id).join(name);
        if path.exists() {
            fs::remove_file(path)?;
            sync_dir(&self.dir(id))?;
        }
        Ok(())
    }
    /// Crash-safe one-way adoption: legacy evidence is deliberately retained.
    pub fn adopt_legacy(&self, i: &Instance) -> Result<Instance> {
        let _lock = self.lock(&i.instance_id)?;
        if self.dir(&i.instance_id).join("instance.json").exists() {
            return self.load(&i.instance_id);
        }
        self.save(i)?;
        let reread = self.load(&i.instance_id)?;
        atomic_json(
            &self.dir(&i.instance_id).join("migration-complete.json"),
            &serde_json::json!({"schema_version":1,"legacy_files_retained":true}),
        )?;
        Ok(reread)
    }
    /// Atomically persists transition in the private instance state directory.
    pub fn save_transition(&self, id: &str, t: &Transition) -> Result<()> {
        t.validate(&self.load(id)?)?;
        atomic_json(&self.dir(id).join("transition.json"), t)
    }
    /// Removes persisted transition, treating an absent file as already complete.
    pub fn clear_transition(&self, id: &str) -> Result<()> {
        let p = self.dir(id).join("transition.json");
        if p.exists() {
            fs::remove_file(p)?
        }
        Ok(())
    }
    /// Lists validated persisted instance identifiers in deterministic order.
    pub fn ids(&self) -> Result<Vec<String>> {
        if !self.root.exists() {
            return Ok(vec![]);
        }
        let mut v = vec![];
        for e in fs::read_dir(&self.root)? {
            let e = e?;
            if e.file_type()?.is_dir()
                && (e.path().join("instance.json").is_file()
                    || e.path().join("current.env.setup").is_file())
            {
                v.push(e.file_name().to_string_lossy().into())
            }
        }
        v.sort();
        Ok(v)
    }
}
/// Atomically replaces a file with `value`, including durable directory synchronization where supported.
fn atomic_bytes(p: &Path, value: &[u8]) -> Result<()> {
    let d = p
        .parent()
        .ok_or_else(|| Error::State("state path has no parent".into()))?;
    fs::create_dir_all(d)?;
    secure_dir(d)?;
    let tmp = d.join(format!(".secret.tmp-{}", Uuid::new_v4()));
    let mut f = OpenOptions::new().write(true).create_new(true).open(&tmp)?;
    secure_file(&f)?;
    f.write_all(value)?;
    f.sync_all()?;
    drop(f);
    // Rename is the visibility commit; syncing the containing directory makes that commit
    // survive a crash after the temporary file's contents have reached stable storage.
    fs::rename(&tmp, p)?;
    sync_dir(d)?;
    Ok(())
}
/// Serializes `v` and atomically persists it at `p`.
fn atomic_json<T: Serialize>(p: &Path, v: &T) -> Result<()> {
    let d = p
        .parent()
        .ok_or_else(|| Error::State("state path has no parent".into()))?;
    fs::create_dir_all(d)?;
    secure_dir(d)?;
    let tmp = d.join(format!(
        ".{}.tmp-{}",
        p.file_name().unwrap().to_string_lossy(),
        Uuid::new_v4()
    ));
    let mut o = OpenOptions::new().write(true).create_new(true).open(&tmp)?;
    secure_file(&o)?;
    serde_json::to_writer_pretty(&mut o, v)?;
    o.write_all(b"\n")?;
    o.sync_all()?;
    drop(o);
    fs::rename(&tmp, p)?;
    sync_dir(d)?;
    Ok(())
}
/// Flushes a directory entry update to durable storage on Unix.
#[cfg(unix)]
fn sync_dir(path: &Path) -> Result<()> {
    File::open(path)?.sync_all()?;
    Ok(())
}
/// Performs no directory sync where `std::fs::File` cannot open directories.
#[cfg(not(unix))]
fn sync_dir(_: &Path) -> Result<()> {
    // Windows does not permit opening a directory through std::fs::File.
    Ok(())
}
/// Sets a state directory to owner-only access on Unix.
#[cfg(unix)]
fn secure_dir(p: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(p, fs::Permissions::from_mode(0o700))?;
    Ok(())
}
/// Leaves directory ACL management to the platform on non-Unix systems.
#[cfg(not(unix))]
fn secure_dir(_: &Path) -> Result<()> {
    Ok(())
}
/// Sets an open state file to owner read/write access on Unix.
#[cfg(unix)]
fn secure_file(f: &File) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    f.set_permissions(fs::Permissions::from_mode(0o600))?;
    Ok(())
}
/// Leaves file ACL management to the platform on non-Unix systems.
#[cfg(not(unix))]
fn secure_file(_: &File) -> Result<()> {
    Ok(())
}
