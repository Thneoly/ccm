use std::process::{Command, Stdio};

use anyhow::{Context, Result};

use crate::{config::AppConfig, credential, provider::ProviderAuth};

pub fn run_claude(config: &AppConfig, model_name: &str) -> Result<()> {
    let model = config
        .models
        .get(model_name)
        .with_context(|| format!("unknown model `{model_name}`"))?;
    let provider = config
        .providers
        .get(&model.provider)
        .with_context(|| format!("unknown provider `{}`", model.provider))?;
    let token = credential::get(&model.provider)?;

    run_command(
        provider.base_url.trim_end_matches('/'),
        &token,
        &model.model_id,
        provider.auth,
    )
}

pub fn run_claude_via_proxy(proxy_url: &str) -> Result<()> {
    run_command(
        proxy_url.trim_end_matches('/'),
        "ccm-local",
        "ccm",
        ProviderAuth::Bearer,
    )
}

fn run_command(base_url: &str, token: &str, model: &str, auth: ProviderAuth) -> Result<()> {
    let mut command = Command::new("claude");
    command
        .env("ANTHROPIC_BASE_URL", base_url)
        .env("ANTHROPIC_MODEL", model)
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("ANTHROPIC_AUTH_TOKEN");

    match auth {
        ProviderAuth::XApiKey => {
            command.env("ANTHROPIC_API_KEY", token);
        }
        ProviderAuth::Bearer => {
            command.env("ANTHROPIC_AUTH_TOKEN", token);
        }
    }

    let status = command
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .context(
            "failed to launch `claude`; make sure Claude Code is installed and available on PATH",
        )?;

    if !status.success() {
        anyhow::bail!("Claude Code exited with status {status}");
    }

    Ok(())
}
