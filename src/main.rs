mod cli;
mod config;
mod control;
mod credential;
mod doctor;
mod health;
mod integrate;
mod launcher;
mod manage;
mod model;
mod provider;
mod proxy;
mod route;
mod routing;
mod state;

use anyhow::{Context, Result};
use clap::Parser;
use cli::{Cli, Command};
use config::AppConfig;
use state::AppState;

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Command::Init { force } => {
            let config_path = AppConfig::init(force)?;
            let state_path = AppState::init(force)?;
            println!("Initialized {}", config_path.display());
            println!("Initialized {}", state_path.display());
        }
        Command::Add { command } => {
            let mut config = load_config()?;
            // Run the legacy `current` migration first: saving the config strips
            // that field, so without migrating up front an unmigrated legacy value
            // would be destroyed instead of moved into state.toml.
            load_state(&config)?;
            manage::handle(&mut config, command)?;
        }
        Command::Integrate { command } => {
            integrate::handle(command)?;
        }
        Command::Doctor => {
            let config = load_config()?;
            doctor::run(&config).await?;
        }
        Command::Proxy { bind } => {
            load_config()?;
            proxy::serve(&bind).await?;
        }
        Command::Auth { command } => {
            credential::handle(command)?;
        }
        Command::List => {
            let config = load_config()?;
            let state = load_state(&config)?;
            for (name, model) in &config.models {
                let marker = if state.current.as_deref() == Some(name.as_str()) {
                    "*"
                } else {
                    " "
                };
                println!(
                    "{} {:16} {:16} {}",
                    marker, name, model.provider, model.model_id
                );
            }
            if !config.routes.is_empty() {
                println!("\nRoutes:");
                for (name, route) in &config.routes {
                    let marker = if state.current.as_deref() == Some(name.as_str()) {
                        "*"
                    } else {
                        " "
                    };
                    println!(
                        "{} {:16} primary={} fallback={}",
                        marker,
                        name,
                        route.primary,
                        if route.fallback.is_empty() {
                            "-".to_string()
                        } else {
                            route.fallback.join(",")
                        }
                    );
                }
            }
        }
        Command::Current => {
            let config = load_config()?;
            let state = load_state(&config)?;
            match &state.current {
                Some(name) => println!("{}", name),
                None => println!("No target selected"),
            }
        }
        Command::Use { target } => {
            let config = load_config()?;
            config.resolve_route(&target)?;
            let mut state = load_state(&config)?;
            state.current = Some(target.clone());
            state.save()?;
            println!("Selected {} as persisted default", target);
        }
        Command::Switch { target, proxy_url } => {
            let proxy_url = proxy_url
                .or_else(|| std::env::var("CCM_PROXY_URL").ok())
                .unwrap_or_else(|| "http://127.0.0.1:13521".to_string());
            control::switch(&proxy_url, &target).await?;
        }
        Command::Run {
            target,
            proxy,
            proxy_url,
        } => {
            let config = load_config()?;
            let state = load_state(&config)?;
            if proxy {
                if let Some(target) = target {
                    control::switch(&proxy_url, &target).await?;
                }
                launcher::run_claude_via_proxy(&proxy_url)?;
            } else {
                let name = match target {
                    Some(target) => config.resolve_target(&target)?,
                    None => {
                        let target = state
                            .current
                            .clone()
                            .context("no current model selected; run `ccm use <name>` first")?;
                        config.resolve_target(&target)?
                    }
                };
                launcher::run_claude(&config, &name)?;
            }
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

fn load_state(config: &AppConfig) -> Result<AppState> {
    AppState::load_or_migrate(config).context("failed to load CCM state")
}
