mod cli;
mod config;
mod credential;
mod health;
mod launcher;
mod model;
mod provider;

use anyhow::{Context, Result};
use clap::Parser;
use cli::{Cli, Command};
use config::AppConfig;

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Command::Init { force } => {
            let path = AppConfig::init(force)?;
            println!("Initialized {}", path.display());
        }
        Command::Auth { command } => {
            credential::handle(command)?;
        }
        Command::List => {
            let config = load_config()?;
            for (name, model) in &config.models {
                let marker = if config.current.as_deref() == Some(name.as_str()) {
                    "*"
                } else {
                    " "
                };
                println!(
                    "{} {:16} {:16} {}",
                    marker, name, model.provider, model.model_id
                );
            }
        }
        Command::Current => {
            let config = load_config()?;
            match &config.current {
                Some(name) => println!("{}", name),
                None => println!("No model selected"),
            }
        }
        Command::Use { target } => {
            let mut config = load_config()?;
            let resolved = config.resolve_target(&target)?;
            config.current = Some(resolved.clone());
            config.save()?;
            println!("Selected {}", resolved);
        }
        Command::Run { target } => {
            let config = load_config()?;
            let name = match target {
                Some(target) => config.resolve_target(&target)?,
                None => config
                    .current
                    .clone()
                    .context("no current model selected; run `ccm use <name>` first")?,
            };
            launcher::run_claude(&config, &name)?;
        }
        Command::Health { target } => {
            let config = load_config()?;
            let name = config.resolve_target(&target)?;
            health::check(&config, &name).await?;
        }
    }

    Ok(())
}

fn load_config() -> Result<AppConfig> {
    AppConfig::load().context("failed to load CCM config")
}
