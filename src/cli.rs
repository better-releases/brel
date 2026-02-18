use clap::{Args, Parser, Subcommand};
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(name = "brel", version, about = "better-releases workflow setup tool")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Debug, Subcommand)]
pub enum Commands {
    /// Initialize release workflow scaffolding.
    Init(InitArgs),
    /// Create or update a release PR.
    ReleasePr(ReleasePrArgs),
    /// Compute the next releasable version.
    NextVersion(NextVersionArgs),
}

#[derive(Debug, Args, Clone)]
pub struct InitArgs {
    /// Path to a config file. Defaults to brel.toml, then .brel.toml in current directory.
    #[arg(long)]
    pub config: Option<PathBuf>,
    /// Run non-interactively and auto-confirm overwrite prompts.
    #[arg(long)]
    pub yes: bool,
    /// Show what would change without writing files.
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(Debug, Args, Clone)]
pub struct ReleasePrArgs {
    /// Path to a config file. Defaults to brel.toml, then .brel.toml in current directory.
    #[arg(long)]
    pub config: Option<PathBuf>,
}

#[derive(Debug, Args, Clone)]
pub struct NextVersionArgs {
    /// Path to a config file. Defaults to brel.toml, then .brel.toml in current directory.
    #[arg(long)]
    pub config: Option<PathBuf>,
}
