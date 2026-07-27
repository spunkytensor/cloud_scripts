use clap::{CommandFactory, Parser, Subcommand, ValueEnum};
use std::path::PathBuf;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, ValueEnum)]
pub enum Output {
    #[default]
    Table,
    Json,
}

#[derive(Debug, Parser)]
#[command(name = "vps", version, about = "Disposable cloud worker control plane")]
pub struct Cli {
    #[arg(long, global = true, default_value = "digitalocean")]
    pub backend: String,
    #[arg(long, global = true, env = "VPS_CONFIG")]
    pub config: Option<PathBuf>,
    #[arg(long, global = true)]
    pub state_dir: Option<PathBuf>,
    #[arg(long, global = true, value_enum, default_value = "table")]
    pub output: Output,
    #[arg(long, global = true)]
    pub verbose: bool,
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Debug, Subcommand)]
pub enum Commands {
    Create {
        #[arg(long, conflicts_with = "instance")]
        new: bool,
        #[arg(long)]
        instance: Option<String>,
        repository: String,
        #[arg(long, default_value = "main")]
        branch: String,
    },
    List {
        #[arg(long)]
        all_backends: bool,
    },
    Status {
        instance: String,
        #[arg(long)]
        refresh: bool,
    },
    Shell {
        instance: String,
    },
    Pause {
        instance: String,
        #[arg(long, alias = "confirm-missing-droplet")]
        confirm_missing_server: bool,
        #[arg(long)]
        confirm_request_not_accepted: bool,
    },
    Resume {
        instance: String,
        #[arg(long)]
        confirm_missing_snapshot: bool,
        #[arg(long)]
        confirm_request_not_accepted: bool,
    },
    Destroy {
        instance: String,
        #[arg(long, alias = "confirm-missing-droplet")]
        confirm_missing_server: bool,
        #[arg(long)]
        confirm_missing_snapshot: bool,
        #[arg(long)]
        confirm_request_not_accepted: bool,
        #[arg(long)]
        forget_unresolved_allocation: bool,
        #[arg(long)]
        forget_unrevoked_token: bool,
    },
    Doctor,
    Completion {
        #[arg(value_enum)]
        shell: clap_complete::Shell,
    },
}

pub fn command() -> clap::Command {
    Cli::command()
}
