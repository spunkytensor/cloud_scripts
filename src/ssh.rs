// SPDX-FileCopyrightText: 2026 Matt Curfman
// SPDX-License-Identifier: Apache-2.0

use crate::{
    error::{Error, Result},
    model::Server,
};
use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::Path,
    process::{Command, ExitStatus, Stdio},
};

pub struct Output {
    pub status: ExitStatus,
    pub stdout: String,
    pub stderr: String,
}
/// Builds an SSH command with strict host-key handling, optionally permitting first-use enrollment.
fn base(server: &Server, key: &Path, known: &Path, tofu: bool) -> Result<Command> {
    let host = server
        .endpoint
        .as_deref()
        .ok_or_else(|| Error::State("server has no public endpoint".into()))?;
    if let Some(parent) = known.parent() {
        fs::create_dir_all(parent)?;
    }
    // Keep trust scoped to this private known-hosts file. TOFU may add a key, but never
    // weakens checking for a host whose key is already enrolled.
    let _file = OpenOptions::new().create(true).append(true).open(known)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        _file.set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    let mut c = Command::new("ssh");
    c.arg("-i")
        .arg(key)
        .args([
            "-o",
            "IdentitiesOnly=yes",
            "-o",
            if tofu {
                "StrictHostKeyChecking=accept-new"
            } else {
                "StrictHostKeyChecking=yes"
            },
            "-o",
            "ConnectTimeout=10",
            "-o",
        ])
        .arg(format!("UserKnownHostsFile={}", known.display()))
        .arg(format!("agent@{host}"));
    Ok(c)
}
/// Runs an SSH command interactively, optionally allocating a TTY, and classifies its exit status.
pub fn run(server: &Server, key: &Path, known: &Path, args: &[&str], tty: bool) -> Result<()> {
    let mut c = base(server, key, known, false)?;
    if tty {
        c.arg("-t");
        c.stdin(Stdio::inherit());
    }
    c.args(args);
    let s = c
        .status()
        .map_err(|e| Error::Remote(format!("could not run OpenSSH: {e}")))?;
    if s.success() {
        Ok(())
    } else {
        Err(Error::Remote(format!("ssh exited with {s}")))
    }
}
/// Runs a remote command with supplied standard input and discards successful output.
pub fn stdin(
    server: &Server,
    key: &Path,
    known: &Path,
    command: &str,
    input: &[u8],
    tofu: bool,
) -> Result<String> {
    let output = stdin_output(server, key, known, command, input, tofu)?;
    if !output.status.success() {
        return Err(Error::Remote(if output.stderr.is_empty() {
            format!("ssh exited with {}", output.status)
        } else {
            output.stderr
        }));
    }
    Ok(output.stdout)
}

/// Runs a remote command with supplied standard input and returns its classified process output.
pub fn stdin_output(
    server: &Server,
    key: &Path,
    known: &Path,
    command: &str,
    input: &[u8],
    tofu: bool,
) -> Result<Output> {
    let mut child = base(server, key, known, tofu)?
        .arg(command)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| Error::Remote(format!("could not run OpenSSH: {e}")))?;
    child
        .stdin
        .take()
        .ok_or_else(|| Error::Remote("could not open OpenSSH stdin".into()))?
        .write_all(input)
        .map_err(|e| Error::Remote(format!("could not write to OpenSSH: {e}")))?;
    let o = child.wait_with_output()?;
    // Preserve the exit status separately: lifecycle probes assign meaning to selected
    // remote exit codes and must not collapse all non-zero exits into transport failure.
    Ok(Output {
        status: o.status,
        stdout: String::from_utf8_lossy(&o.stdout).trim().into(),
        stderr: String::from_utf8_lossy(&o.stderr).trim().into(),
    })
}
/// Runs a remote command and returns UTF-8 standard output after successful exit classification.
pub fn capture(server: &Server, key: &Path, known: &Path, command: &str) -> Result<String> {
    let o = base(server, key, known, false)?
        .arg(command)
        .output()
        .map_err(|e| Error::Remote(format!("could not run OpenSSH: {e}")))?;
    if !o.status.success() {
        return Err(Error::Remote(
            String::from_utf8_lossy(&o.stderr).trim().into(),
        ));
    }
    Ok(String::from_utf8_lossy(&o.stdout).trim().into())
}
