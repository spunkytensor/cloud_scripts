use clap::Parser;
use std::{fs, io};
use vps_control_plane::{
    backend::Backend,
    backend::digitalocean::DigitalOcean,
    cli::{self, Cli, Commands, ConfigCommand, Output},
    config::{self, Config},
    error::{Error, Result},
    lifecycle::{ControlPlane, DestroyOptions, PauseOptions, ResumeOptions},
    model::Lifecycle as State,
    state::Store,
};

#[tokio::main]
async fn main() {
    if let Err(e) = run().await {
        eprintln!("error: {e}");
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
    if matches!(cli.command, Commands::Man) {
        clap_mangen::Man::new(cli::command())
            .render(&mut io::stdout())
            .map_err(Error::Io)?;
        return Ok(());
    }
    if let Commands::Config {
        command: ConfigCommand::Migrate { source, output },
    } = &cli.command
    {
        let text = config::migrate_legacy(source)?;
        if let Some(p) = output {
            fs::write(p, text)?
        } else {
            print!("{text}")
        }
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
            show(&life.create(id, repository, branch).await?, cli.output)?
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
            println!(
                "Destroyed {instance}; provider deletion and credential cleanup were confirmed."
            )
        }
        Commands::Doctor => {
            backend.validate_access().await?;
            println!(
                "configuration: ok\nstate: {}\nbackend digitalocean: ok",
                root.display()
            )
        }
        Commands::Status {
            instance,
            refresh: true,
        } => {
            let i = store.load_or_adopt(&instance)?;
            show(&i, cli.output)?
        }
        Commands::List { .. }
        | Commands::Shell { .. }
        | Commands::Status { refresh: false, .. }
        | Commands::Completion { .. }
        | Commands::Man
        | Commands::Config { .. } => unreachable!(),
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
                    Err(e) => eprintln!("{id}: {e}"),
                }
            }
            if cli.output == Output::Json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(
                        &serde_json::json!({"schema_version":1,"instances":rows})
                    )?
                )
            } else {
                println!("INSTANCE\tBACKEND\tSTATUS\tRESOURCE_ID\tIP\tSNAPSHOT_ID\tSOURCE\tBRANCH");
                for i in rows {
                    let (status, r, ip, snap) =
                        summary(&i, s.transition(&i.instance_id).ok().flatten().as_ref());
                    println!(
                        "{}\t{}\t{}\t{}\t{}\t{}\t{}@{}\t{}",
                        i.instance_id,
                        i.backend,
                        status,
                        r,
                        ip,
                        snap,
                        i.repository,
                        i.base_branch,
                        i.work_branch
                    )
                }
            }
        }
        Commands::Status { instance, .. } => {
            let i = load_or_import(s, &instance)?;
            show(&i, cli.output)?
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
                .ok_or_else(|| Error::Cli("ssh.private_key is required".into()))?;
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
fn show<T: serde::Serialize>(v: &T, _output: Output) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(v)?);
    Ok(())
}
fn summary(
    i: &vps_control_plane::model::Instance,
    t: Option<&vps_control_plane::model::Transition>,
) -> (String, String, String, String) {
    if let Some(t) = t {
        return (
            format!("{:?}", t.phase).to_lowercase(),
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
