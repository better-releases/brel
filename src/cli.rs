use clap::{Args, Parser, Subcommand};
use semver::Version;
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
    /// Generate the configured changelog for the next release.
    Changelog(ChangelogArgs),
    /// Create or update a release PR.
    ReleasePr(ReleasePrArgs),
    /// Create and push a release tag.
    Tag(TagArgs),
    /// Compute the next releasable version.
    NextVersion(NextVersionArgs),
    /// Post a projected-version preview comment on a pull request.
    PreviewComment(PreviewCommentArgs),
    /// Validate a config file.
    Validate(ValidateArgs),
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
    /// Force a stable release version instead of computing a bump.
    #[arg(long, value_parser = Version::parse)]
    pub release_as: Option<Version>,
}

#[derive(Debug, Args, Clone)]
pub struct ChangelogArgs {
    /// Path to a config file. Defaults to brel.toml, then .brel.toml in current directory.
    #[arg(long)]
    pub config: Option<PathBuf>,
    /// Force a stable release version instead of computing a bump.
    #[arg(long, value_parser = Version::parse)]
    pub release_as: Option<Version>,
}

#[derive(Debug, Args, Clone)]
pub struct TagArgs {
    /// Path to a config file. Defaults to brel.toml, then .brel.toml in current directory.
    #[arg(long)]
    pub config: Option<PathBuf>,
    /// Release tag to create. When omitted, provider merge event data is used.
    #[arg(long)]
    pub tag: Option<String>,
    /// Git revision to tag. Defaults to HEAD in manual mode or the PR merge commit in event mode.
    #[arg(long)]
    pub target: Option<String>,
}

#[derive(Debug, Args, Clone)]
pub struct NextVersionArgs {
    /// Path to a config file. Defaults to brel.toml, then .brel.toml in current directory.
    #[arg(long)]
    pub config: Option<PathBuf>,
    /// Force a stable release version instead of computing a bump.
    #[arg(long, value_parser = Version::parse)]
    pub release_as: Option<Version>,
}

#[derive(Debug, Args, Clone)]
pub struct PreviewCommentArgs {
    /// Path to a config file. Defaults to brel.toml, then .brel.toml in current directory.
    #[arg(long)]
    pub config: Option<PathBuf>,
    /// Pull request number. When omitted, the GitHub `pull_request` event is used.
    #[arg(long)]
    pub pr_number: Option<u64>,
    /// Force a stable release version instead of computing a bump.
    #[arg(long, value_parser = Version::parse)]
    pub release_as: Option<Version>,
}

#[derive(Debug, Args, Clone)]
pub struct ValidateArgs {
    /// Path to a config file. Defaults to brel.toml, then .brel.toml in current directory.
    #[arg(long)]
    pub config: Option<PathBuf>,
}
