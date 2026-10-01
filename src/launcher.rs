use std::process::{Command, Stdio};

use anyhow::{Context, Result};

use crate::{config::AppConfig, credential};

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
    )
}

pub fn run_claude_via_proxy(proxy_url: &str) -> Result<()> {
    run_command(proxy_url.trim_end_matches('/'), "ccm-local", "ccm")
}

fn run_command(base_url: &str, token: &str, model: &str) -> Result<()> {
    let status = Command::new("claude")
        .env("ANTHROPIC_BASE_URL", base_url)
        .env("ANTHROPIC_AUTH_TOKEN", token)
        .env("ANTHROPIC_MODEL", model)
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
