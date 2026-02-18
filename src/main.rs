mod cli;
mod config;
mod init;
mod release_pr;
mod tag_template;
mod template;
mod version_update;
mod workflow;

use anyhow::Result;
use clap::Parser;
use cli::{Cli, Commands};

fn main() {
    if let Err(err) = run() {
        eprintln!("Error: {err:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Commands::Init(args) => init::run(args),
        Commands::ReleasePr(args) => release_pr::run(args),
        Commands::NextVersion(args) => release_pr::run_next_version(args),
    }
}
