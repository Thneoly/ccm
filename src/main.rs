mod cli;
mod config;
mod control;
mod credential;
mod doctor;
mod health;
mod history;
mod history_cli;
mod integrate;
mod launcher;
mod manage;
mod model;
mod provider;
mod proxy;
mod route;
mod routing;
mod state;
mod translate;

use anyhow::{bail, Context, Result};
use clap::Parser;
use cli::{Cli, Command};
use config::AppConfig;
use state::AppState;

const DEFAULT_PROXY_URL: &str = "http://127.0.0.1:13521";

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
        Command::Switch {
            target,
            proxy_url,
            client,
            global,
        } => {
            let proxy_url = resolve_proxy_url(proxy_url, std::env::var("CCM_PROXY_URL").ok());
            let env_client = std::env::var("CCM_CLIENT_ID").ok();
            let client = resolve_switch_client(client.as_deref(), global, env_client.as_deref());
            if let Some(id) = client {
                ensure_valid_client_id(id)?;
            }
            control::switch(&proxy_url, &target, client).await?;
        }
        Command::Clients { proxy_url } => {
            let proxy_url = resolve_proxy_url(proxy_url, std::env::var("CCM_PROXY_URL").ok());
            control::clients(&proxy_url).await?;
        }
        Command::Run {
            target,
            proxy,
            proxy_url,
            client,
        } => {
            let config = load_config()?;
            let state = load_state(&config)?;
            if proxy {
                let client_id = client
                    .or_else(|| std::env::var("CCM_CLIENT_ID").ok())
                    .unwrap_or_else(launcher::short_client_id);
                ensure_valid_client_id(&client_id)?;
                if let Some(target) = target {
                    // Strictly client-scoped pre-switch (V0.4_PLAN 8.1
                    // decision 3): a per-session target never moves the
                    // global runtime target.
                    control::switch(&proxy_url, &target, Some(&client_id)).await?;
                }
                launcher::run_claude_via_proxy(&proxy_url, &client_id)?;
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
                if launcher::is_openai_compatible_model(&config, &name) {
                    bail!(
                        "cannot launch `{name}` directly: {}",
                        launcher::PROXY_ONLY_REASON
                    );
                }
                launcher::run_claude(&config, &name)?;
            }
        }
        Command::Health { target } => {
            let config = load_config()?;
            let name = config.resolve_target(&target)?;
            health::check(&config, &name).await?;
        }
        Command::History { command } => history_cli::run(command)?,
    }

    Ok(())
}

fn load_config() -> Result<AppConfig> {
    AppConfig::load().context("failed to load CCM config")
}

fn load_state(config: &AppConfig) -> Result<AppState> {
    AppState::load_or_migrate(config).context("failed to load CCM state")
}

/// Proxy URL resolution shared by `ccm switch` / `ccm clients`:
/// `--proxy-url` flag > `CCM_PROXY_URL` env > default. Pure (env passed in)
/// so tests never mutate process environment.
fn resolve_proxy_url(flag: Option<String>, env: Option<String>) -> String {
    flag.or(env)
        .unwrap_or_else(|| DEFAULT_PROXY_URL.to_string())
}

/// `ccm switch` client resolution: `--client` > `CCM_CLIENT_ID` env > global
/// (None). `--global` forces the global target even when the env var is set
/// (clap already rejects `--client` + `--global` together).
fn resolve_switch_client<'a>(
    flag: Option<&'a str>,
    global: bool,
    env: Option<&'a str>,
) -> Option<&'a str> {
    if global {
        return None;
    }
    flag.or(env)
}

/// Client-side mirror of the proxy's switch validation (`apply_switch`) so
/// an invalid id fails before any request is sent. The message comes from
/// the same helper the control API uses, so both stay identical by
/// construction.
fn ensure_valid_client_id(id: &str) -> Result<()> {
    if proxy::valid_client_id(id) {
        Ok(())
    } else {
        bail!(proxy::invalid_client_id_message(id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proxy_url_resolution_order() {
        assert_eq!(
            resolve_proxy_url(
                Some("http://127.0.0.1:13599".to_string()),
                Some("http://127.0.0.1:1".to_string())
            ),
            "http://127.0.0.1:13599"
        );
        assert_eq!(
            resolve_proxy_url(None, Some("http://127.0.0.1:1".to_string())),
            "http://127.0.0.1:1"
        );
        assert_eq!(resolve_proxy_url(None, None), DEFAULT_PROXY_URL);
    }

    #[test]
    fn switch_client_resolution_order() {
        assert_eq!(
            resolve_switch_client(Some("a"), false, Some("b")),
            Some("a"),
            "--client wins over CCM_CLIENT_ID"
        );
        assert_eq!(
            resolve_switch_client(None, false, Some("b")),
            Some("b"),
            "CCM_CLIENT_ID applies when --client is absent"
        );
        assert_eq!(
            resolve_switch_client(None, false, None),
            None,
            "no id anywhere means the global target"
        );
        assert_eq!(
            resolve_switch_client(Some("a"), true, Some("b")),
            None,
            "--global forces the global target even with an id present"
        );
    }

    #[test]
    fn invalid_client_ids_fail_with_the_control_api_message_shape() {
        assert!(ensure_valid_client_id("a").is_ok());
        assert!(ensure_valid_client_id("A.b-1_2").is_ok());
        assert!(ensure_valid_client_id(&"x".repeat(64)).is_ok());
        for id in ["", "bad id", "id/1", &"x".repeat(65)] {
            // Byte-identical to the control API's rejection text.
            assert_eq!(
                ensure_valid_client_id(id).unwrap_err().to_string(),
                proxy::invalid_client_id_message(id),
                "{id}"
            );
        }
    }
}
