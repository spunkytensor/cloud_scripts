// SPDX-FileCopyrightText: 2026 Matt Curfman
// SPDX-License-Identifier: Apache-2.0

use clap::Parser;
use std::{io, path::Path};
use unicode_width::UnicodeWidthStr;
use vps_control_plane::{
    backend::Backend,
    backend::digitalocean::DigitalOcean,
    cli::{self, Cli, Commands, Output},
    config::Config,
    console,
    error::{Error, Result},
    lifecycle::{ControlPlane, DestroyOptions, PauseOptions, ResumeOptions},
    model::Lifecycle as State,
    state::Store,
};

/// Runs the asynchronous command-line entry point and converts failures into process exit status.
#[tokio::main]
async fn main() {
    if let Err(e) = run().await {
        console::error(e.to_string());
        std::process::exit(e.exit_code().into())
    }
}
/// Loads configuration, dispatches the selected command, and renders its result.
async fn run() -> Result<()> {
    let cli = Cli::parse();
    if let Commands::Completion { shell } = &cli.command {
        let mut c = cli::command();
        clap_complete::generate(*shell, &mut c, "vps", &mut io::stdout());
        return Ok(());
    }
    let (cfg, root) = Config::load(cli.config.as_deref(), cli.home.clone())?;
    let store = Store::new(root.clone());
    if matches!(
        cli.command,
        Commands::List | Commands::Status { refresh: false, .. } | Commands::Shell { .. }
    ) {
        return local(cli, &cfg, &store);
    }
    let backend_name = match &cli.command {
        Commands::Create { backend, .. } => {
            backend.as_ref().unwrap_or(&cfg.default_backend).clone()
        }
        Commands::Doctor => cfg.default_backend.clone(),
        Commands::Pause { instance, .. }
        | Commands::Resume { instance, .. }
        | Commands::Destroy { instance, .. }
        | Commands::Status {
            instance,
            refresh: true,
        } => store.load_or_adopt(instance)?.backend,
        Commands::List | Commands::Shell { .. } | Commands::Completion { .. } => unreachable!(),
        Commands::Status { refresh: false, .. } => unreachable!(),
    };
    if backend_name != "digitalocean" {
        return Err(Error::Cli(format!(
            "backend {backend_name} is not available; compiled backends: digitalocean"
        )));
    }
    let backend = DigitalOcean::new(&cfg.backends.digitalocean)?;
    let life = ControlPlane {
        store: &store,
        config: &cfg,
        backend: &backend,
    };
    match cli.command {
        Commands::Create {
            name,
            backend: _,
            repository,
            branch,
        } => {
            let id = name.unwrap_or_else(generated_id);
            show(
                &life.create(&backend_name, id, repository, branch).await?,
                cli.output,
                cli.verbose,
                cli.config.as_deref(),
                cli.home.as_deref(),
            )?
        }
        Commands::Pause {
            instance,
            confirm_missing_server,
            confirm_request_not_accepted,
        } => show(
            &life
                .pause(
                    &instance,
                    PauseOptions {
                        confirm_missing_server,
                        confirm_request_not_accepted,
                    },
                )
                .await?,
            cli.output,
            cli.verbose,
            cli.config.as_deref(),
            cli.home.as_deref(),
        )?,
        Commands::Resume {
            instance,
            confirm_missing_snapshot,
            confirm_request_not_accepted,
        } => show(
            &life
                .resume(
                    &instance,
                    ResumeOptions {
                        confirm_missing_snapshot,
                        confirm_request_not_accepted,
                    },
                )
                .await?,
            cli.output,
            cli.verbose,
            cli.config.as_deref(),
            cli.home.as_deref(),
        )?,
        Commands::Destroy {
            instance,
            confirm_missing_server,
            confirm_missing_snapshot,
            confirm_request_not_accepted,
            forget_unresolved_allocation,
            forget_unrevoked_token,
        } => {
            let outcome = life
                .destroy(
                    &instance,
                    DestroyOptions {
                        confirm_missing_server,
                        confirm_missing_snapshot,
                        confirm_request_not_accepted,
                        forget_unresolved_allocation,
                        forget_unrevoked_token,
                    },
                )
                .await?;
            if outcome.credentials_revoked {
                console::success(
                    "Instance",
                    format!("{instance} destroyed; provider and credential cleanup confirmed"),
                )
            } else {
                console::action(
                    "Instance",
                    format!(
                        "{instance} destroyed; provider cleanup confirmed, credential revocation was explicitly forgotten"
                    ),
                )
            }
        }
        Commands::Doctor => {
            console::pending("Doctor", "Checking DigitalOcean API access");
            backend.validate_access().await?;
            backend
                .resolve_ssh_key(cfg.backends.digitalocean.ssh_key.as_deref())
                .await?;
            console::success("Doctor", "Configuration and DigitalOcean access verified");
            if cli.verbose {
                println!("  State directory  {}", root.display());
            }
        }
        Commands::Status {
            instance,
            refresh: true,
        } => {
            let i = refresh_status(store.load_or_adopt(&instance)?, &backend).await?;
            show(
                &i,
                cli.output,
                cli.verbose,
                cli.config.as_deref(),
                cli.home.as_deref(),
            )?
        }
        Commands::List
        | Commands::Shell { .. }
        | Commands::Status { refresh: false, .. }
        | Commands::Completion { .. } => unreachable!(),
    }
    Ok(())
}
/// Handles commands that need only persisted state, rendering lists/status or opening SSH.
fn local(cli: Cli, cfg: &Config, s: &Store) -> Result<()> {
    match cli.command {
        Commands::List => {
            let mut rows = vec![];
            for id in s.ids()? {
                match s.load_or_adopt(&id) {
                    Ok(i) => rows.push(i),
                    Err(e) => console::error(format!("{id}: {e}")),
                }
            }
            if cli.output == Output::Json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(
                        &serde_json::json!({"schema_version":1,"instances":rows})
                    )?
                )
            } else if cli.verbose {
                let mut table_rows = Vec::with_capacity(rows.len());
                for i in rows {
                    let (status, resource, address, snapshot) =
                        summary(&i, s.transition(&i.instance_id).ok().flatten().as_ref());
                    table_rows.push([
                        i.instance_id,
                        i.backend,
                        status,
                        resource,
                        address,
                        snapshot,
                        format!("{}@{}", i.repository, i.base_branch),
                        i.work_branch,
                    ]);
                }
                println!(
                    "{}",
                    table(
                        [
                            "INSTANCE",
                            "BACKEND",
                            "STATUS",
                            "RESOURCE",
                            "ADDRESS",
                            "SNAPSHOT",
                            "REPOSITORY",
                            "WORK BRANCH",
                        ],
                        &table_rows,
                    )
                );
            } else {
                let mut table_rows = Vec::with_capacity(rows.len());
                for i in rows {
                    let (status, _, _, _) =
                        summary(&i, s.transition(&i.instance_id).ok().flatten().as_ref());
                    table_rows.push([
                        i.instance_id,
                        i.backend,
                        status,
                        format!("{}@{}", i.repository, i.base_branch),
                        i.work_branch,
                    ]);
                }
                println!(
                    "{}",
                    table(
                        ["INSTANCE", "BACKEND", "STATUS", "REPOSITORY", "WORK BRANCH"],
                        &table_rows,
                    )
                );
            }
        }
        Commands::Status { instance, .. } => {
            let i = s.load_or_adopt(&instance)?;
            show(
                &i,
                cli.output,
                cli.verbose,
                cli.config.as_deref(),
                cli.home.as_deref(),
            )?
        }
        Commands::Shell { instance } => {
            let i = s.load_or_adopt(&instance)?;
            if let Some(t) = s.transition(&instance)?
                && !t.completed_for(&i)
                && t.phase != vps_control_plane::model::Phase::ActiveSnapshotCleanupPending
            {
                return Err(Error::State(format!(
                    "instance is {:?}; resume or destroy it",
                    t.phase
                )));
            }
            let server = match i.lifecycle {
                State::Active { server, .. } => server,
                _ => {
                    return Err(Error::State(format!(
                        "instance is paused; resume it with: vps resume {instance}"
                    )));
                }
            };
            let key = cfg
                .ssh
                .private_key
                .as_deref()
                .ok_or_else(|| {
                    Error::Cli(
                        "ssh.private_key is required; pass the same --config used to create the instance"
                            .into(),
                    )
                })?;
            console::action(
                "Shell",
                format!(
                    "Opening an interactive session to {}",
                    server.endpoint.as_deref().unwrap_or("the worker")
                ),
            );
            vps_control_plane::ssh::run(
                &server,
                key,
                &s.dir(&instance).join("known_hosts"),
                &[],
                true,
            )?
        }
        _ => unreachable!(),
    }
    Ok(())
}
/// Generates a collision-resistant worker identifier suitable for persisted state paths.
fn generated_id() -> String {
    format!("worker-{}", uuid::Uuid::new_v4().simple())
}
/// Verifies persisted resources against the provider and refreshes active server metadata.
/// Missing owned resources and ambiguous allocation correlations are reported as uncertain.
async fn refresh_status(
    mut instance: vps_control_plane::model::Instance,
    backend: &dyn Backend,
) -> Result<vps_control_plane::model::Instance> {
    console::pending("Provider", "Refreshing provider state");
    match &mut instance.lifecycle {
        State::Active { server, snapshot } => {
            *server = backend
                .get_server(&server.id)
                .await?
                .ok_or_else(|| Error::Uncertain("active provider server is missing".into()))?;
            if let Some(snapshot) = snapshot
                && backend.get_snapshot(&snapshot.id).await?.is_none()
            {
                return Err(Error::Uncertain(
                    "cleanup-pending provider snapshot is missing".into(),
                ));
            }
        }
        State::Paused { snapshot } => {
            if backend.get_snapshot(&snapshot.id).await?.is_none() {
                return Err(Error::Uncertain(
                    "paused provider snapshot is missing".into(),
                ));
            }
        }
        State::AllocationPending { correlation, .. } => {
            let matches = backend.find_servers(correlation).await?;
            if matches.len() > 1 {
                return Err(Error::Uncertain(format!(
                    "allocation correlation has {} provider matches",
                    matches.len()
                )));
            }
        }
        State::SourceReserved { .. } => {}
    }
    console::success("Provider", "Provider state refreshed");
    Ok(instance)
}
/// Renders a left-aligned plain-text table using Unicode display widths.
fn table<const N: usize>(headers: [&str; N], rows: &[[String; N]]) -> String {
    let widths: [usize; N] = std::array::from_fn(|column| {
        rows.iter()
            .map(|row| UnicodeWidthStr::width(row[column].as_str()))
            .chain([UnicodeWidthStr::width(headers[column])])
            .max()
            .unwrap_or(0)
    });
    let line = |cells: [&str; N]| {
        cells
            .iter()
            .enumerate()
            .map(|(column, cell)| {
                if column + 1 == N {
                    (*cell).to_owned()
                } else {
                    format!(
                        "{cell}{}",
                        " ".repeat(widths[column] + 2 - UnicodeWidthStr::width(*cell))
                    )
                }
            })
            .collect::<String>()
    };
    let mut lines = vec![line(headers)];
    lines.extend(
        rows.iter()
            .map(|row| line(row.each_ref().map(String::as_str))),
    );
    lines.join("\n")
}
/// Prints one instance as schema-versioned JSON or human-readable status details.
fn show(
    i: &vps_control_plane::model::Instance,
    output: Output,
    verbose: bool,
    config: Option<&Path>,
    home: Option<&Path>,
) -> Result<()> {
    if output == Output::Json {
        println!("{}", serde_json::to_string_pretty(i)?);
        return Ok(());
    }

    let (status, resource, endpoint, snapshot) = summary(i, None);
    console::success("Instance", format!("{} is {status}", i.instance_id));
    if status == "active" {
        println!(
            "  Connect         {}",
            shell_command(&i.instance_id, config, home)
        );
    }
    if !verbose {
        return Ok(());
    }
    println!("  Repository      {}@{}", i.repository, i.base_branch);
    println!("  Work branch     {}", i.work_branch);
    println!("  Backend         {}", i.backend);
    if resource != "-" {
        println!("  Resource        {resource}");
    }
    if endpoint != "-" {
        println!("  Address         {endpoint}");
    }
    if snapshot != "-" {
        println!("  Snapshot        {snapshot}");
    }
    Ok(())
}
/// Builds a copyable `vps shell` command preserving explicit config and home paths.
fn shell_command(id: &str, config: Option<&Path>, home: Option<&Path>) -> String {
    let mut command = String::from("vps shell");
    if let Some(path) = config {
        command.push_str(" --config ");
        command.push_str(&shell_arg(path));
    }
    if let Some(path) = home {
        command.push_str(" --home ");
        command.push_str(&shell_arg(path));
    }
    command.push(' ');
    command.push_str(id);
    command
}
/// Single-quotes a path for safe use as one POSIX shell argument.
fn shell_arg(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"))
}
/// Derives display status and provider identifiers, preferring an in-progress transition.
fn summary(
    i: &vps_control_plane::model::Instance,
    t: Option<&vps_control_plane::model::Transition>,
) -> (String, String, String, String) {
    if let Some(t) = t {
        return (
            phase_status(&t.phase).into(),
            t.target
                .as_ref()
                .or(t.source.as_ref())
                .map(|s| s.id.clone())
                .unwrap_or_else(|| "-".into()),
            t.target
                .as_ref()
                .and_then(|s| s.endpoint.clone())
                .unwrap_or_else(|| "-".into()),
            t.snapshot
                .as_ref()
                .map(|s| s.id.clone())
                .unwrap_or_else(|| "-".into()),
        );
    }
    match &i.lifecycle {
        State::SourceReserved { .. } => {
            ("source-reserved".into(), "-".into(), "-".into(), "-".into())
        }
        State::AllocationPending { .. } => (
            "allocation-pending".into(),
            "-".into(),
            "-".into(),
            "-".into(),
        ),
        State::Active { server, snapshot } => (
            if snapshot.is_some() {
                "active-snapshot-cleanup-pending".into()
            } else {
                "active".into()
            },
            server.id.clone(),
            server.endpoint.clone().unwrap_or_else(|| "-".into()),
            snapshot
                .as_ref()
                .map(|s| s.id.clone())
                .unwrap_or_else(|| "-".into()),
        ),
        State::Paused { snapshot } => {
            ("paused".into(), "-".into(), "-".into(), snapshot.id.clone())
        }
    }
}
/// Maps a persisted transition phase to its stable CLI status spelling.
fn phase_status(phase: &vps_control_plane::model::Phase) -> &'static str {
    use vps_control_plane::model::Phase;
    match phase {
        Phase::PausingQuiescing => "pausing-quiescing",
        Phase::PausingShutdown => "pausing-shutdown",
        Phase::PausingSnapshot => "pausing-snapshot",
        Phase::PausingDeletePending => "pausing-delete-pending",
        Phase::ResumingAllocation => "resuming-allocation",
        Phase::ResumingRecovery => "resuming-recovery",
        Phase::ActiveSnapshotCleanupPending => "active-snapshot-cleanup-pending",
        Phase::Destroying => "destroying",
    }
}

#[cfg(test)]
mod tests {
    use super::table;
    use unicode_width::UnicodeWidthStr;

    /// Table columns align to the widest header or cell.
    #[test]
    fn table_aligns_columns_to_longest_value() {
        let output = table(
            ["INSTANCE", "STATUS", "WORK BRANCH"],
            &[[
                "worker-b529407b11424f539d7ce634b0e1c7bb".into(),
                "active".into(),
                "codex/worker-b529407b11424f539d7ce634b0e1c7bb".into(),
            ]],
        );

        let lines: Vec<_> = output.lines().collect();
        assert_eq!(lines[0].find("STATUS"), lines[1].find("active"));
        assert_eq!(lines[0].find("WORK BRANCH"), lines[1].find("codex/"));
    }

    /// Table alignment uses terminal display width rather than UTF-8 byte length.
    #[test]
    fn table_aligns_columns_by_unicode_display_width() {
        let output = table(["NAME", "STATUS"], &[["開発".into(), "active".into()]]);
        let lines: Vec<_> = output.lines().collect();
        let header_offset = UnicodeWidthStr::width(&lines[0][..lines[0].find("STATUS").unwrap()]);
        let row_offset = UnicodeWidthStr::width(&lines[1][..lines[1].find("active").unwrap()]);
        assert_eq!(header_offset, row_offset);
    }
}
