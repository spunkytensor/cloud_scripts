use crate::{
    error::{Error, Result},
    model::Server,
};
use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::Path,
    process::{Command, Stdio},
};
fn base(server: &Server, key: &Path, known: &Path, tofu: bool) -> Result<Command> {
    let host = server
        .endpoint
        .as_deref()
        .ok_or_else(|| Error::State("server has no public endpoint".into()))?;
    if let Some(parent) = known.parent() {
        fs::create_dir_all(parent)?;
    }
    let file = OpenOptions::new().create(true).append(true).open(known)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
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
pub fn stdin(
    server: &Server,
    key: &Path,
    known: &Path,
    command: &str,
    input: &[u8],
    tofu: bool,
) -> Result<String> {
    let mut child = base(server, key, known, tofu)?
        .arg(command)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    child.stdin.take().unwrap().write_all(input)?;
    let o = child.wait_with_output()?;
    if !o.status.success() {
        return Err(Error::Remote(
            String::from_utf8_lossy(&o.stderr).trim().into(),
        ));
    }
    Ok(String::from_utf8_lossy(&o.stdout).trim().into())
}
pub fn capture(server: &Server, key: &Path, known: &Path, command: &str) -> Result<String> {
    let host = server
        .endpoint
        .as_deref()
        .ok_or_else(|| Error::State("server has no endpoint".into()))?;
    let o = Command::new("ssh")
        .args(["-i"])
        .arg(key)
        .args([
            "-o",
            "IdentitiesOnly=yes",
            "-o",
            "StrictHostKeyChecking=yes",
            "-o",
        ])
        .arg(format!("UserKnownHostsFile={}", known.display()))
        .arg(format!("agent@{host}"))
        .arg(command)
        .output()?;
    if !o.status.success() {
        return Err(Error::Remote(
            String::from_utf8_lossy(&o.stderr).trim().into(),
        ));
    }
    Ok(String::from_utf8_lossy(&o.stdout).trim().into())
}
