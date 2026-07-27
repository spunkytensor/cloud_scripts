use clap::Parser;
use std::{io, path::Path};
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

#[tokio::main]
async fn main() {
    if let Err(e) = run().await {
        console::error(e.to_string());
        std::process::exit(e.exit_code().into())
    }
}
async fn run() -> Result<()> {
    let cli = Cli::parse();
    if cli.backend != "digitalocean" {
        return Err(Error::Cli(
            "unknown backend; compiled backends: digitalocean".into(),
        ));
    }
    if let Commands::Completion { shell } = &cli.command {
        let mut c = cli::command();
        clap_complete::generate(*shell, &mut c, "vps", &mut io::stdout());
        return Ok(());
    }
    let (cfg, root) = Config::load(cli.config.as_deref(), cli.state_dir.clone())?;
    let store = Store::new(root.clone());
    if matches!(
        cli.command,
        Commands::List { .. } | Commands::Status { refresh: false, .. } | Commands::Shell { .. }
    ) {
        return local(cli, &cfg, &store);
    }
    let backend = DigitalOcean::new(&cfg.backends.digitalocean)?;
    let life = ControlPlane {
        store: &store,
        config: &cfg,
        backend: &backend,
    };
    match cli.command {
        Commands::Create {
            new,
            instance,
            repository,
            branch,
        } => {
            let id = instance
                .or_else(|| new.then(generated_id))
                .ok_or_else(|| Error::Cli("create requires --new or --instance".into()))?;
            show(
                &life.create(id, repository, branch).await?,
                cli.output,
                cli.verbose,
                cli.config.as_deref(),
                cli.state_dir.as_deref(),
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
            cli.state_dir.as_deref(),
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
            cli.state_dir.as_deref(),
        )?,
        Commands::Destroy {
            instance,
            confirm_missing_server,
            confirm_missing_snapshot,
            confirm_request_not_accepted,
            forget_unresolved_allocation,
            forget_unrevoked_token,
        } => {
            life.destroy(
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
            console::success(
                "Instance",
                format!("{instance} destroyed; provider and credential cleanup confirmed"),
            )
        }
        Commands::Doctor => {
            console::pending("Doctor", "Checking DigitalOcean API access");
            backend.validate_access().await?;
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
                cli.state_dir.as_deref(),
            )?
        }
        Commands::List { .. }
        | Commands::Shell { .. }
        | Commands::Status { refresh: false, .. }
        | Commands::Completion { .. } => unreachable!(),
    }
    Ok(())
}
fn local(cli: Cli, cfg: &Config, s: &Store) -> Result<()> {
    match cli.command {
        Commands::List { .. } => {
            let mut rows = vec![];
            for id in s.ids()? {
                match load_or_import(s, &id) {
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
                        status,
                        format!("{}@{}", i.repository, i.base_branch),
                        i.work_branch,
                    ]);
                }
                println!(
                    "{}",
                    table(
                        ["INSTANCE", "STATUS", "REPOSITORY", "WORK BRANCH"],
                        &table_rows,
                    )
                );
            }
        }
        Commands::Status { instance, .. } => {
            let i = load_or_import(s, &instance)?;
            show(
                &i,
                cli.output,
                cli.verbose,
                cli.config.as_deref(),
                cli.state_dir.as_deref(),
            )?
        }
        Commands::Shell { instance } => {
            let i = load_or_import(s, &instance)?;
            if let Some(t) = s.transition(&instance)?
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
fn load_or_import(s: &Store, id: &str) -> Result<vps_control_plane::model::Instance> {
    s.load_or_adopt(id)
}
fn generated_id() -> String {
    format!("worker-{}", uuid::Uuid::new_v4().simple())
}
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
fn table<const N: usize>(headers: [&str; N], rows: &[[String; N]]) -> String {
    let widths: [usize; N] = std::array::from_fn(|column| {
        rows.iter()
            .map(|row| row[column].len())
            .chain([headers[column].len()])
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
                    format!("{cell:<width$}", width = widths[column] + 2)
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
fn show(
    i: &vps_control_plane::model::Instance,
    output: Output,
    verbose: bool,
    config: Option<&Path>,
    state_dir: Option<&Path>,
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
            shell_command(&i.instance_id, config, state_dir)
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
fn shell_command(id: &str, config: Option<&Path>, state_dir: Option<&Path>) -> String {
    let mut command = String::from("vps shell");
    if let Some(path) = config {
        command.push_str(" --config ");
        command.push_str(&shell_arg(path));
    }
    if let Some(path) = state_dir {
        command.push_str(" --state-dir ");
        command.push_str(&shell_arg(path));
    }
    command.push(' ');
    command.push_str(id);
    command
}
fn shell_arg(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"))
}
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
}
