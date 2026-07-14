use crate::cli::ValidateArgs;
use crate::config;
use anyhow::{Context, Result, bail};

pub fn run(args: &ValidateArgs) -> Result<()> {
    let cwd = std::env::current_dir().context("Failed to determine current directory.")?;
    let config = config::load(args.config.as_deref(), &cwd)?;

    for warning in &config.warnings {
        eprintln!("warning: {warning}");
    }

    let Some(path) = config.source.path() else {
        bail!("No config file found to validate. Create `brel.toml` or pass `--config <path>`.");
    };

    println!("Config file `{}` is valid.", path.display());
    Ok(())
}
