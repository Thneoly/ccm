use anyhow::{Context, Result};
use reqwest::StatusCode;

use crate::{config::AppConfig, credential, provider::ProviderKind};

pub async fn check(config: &AppConfig, model_name: &str) -> Result<()> {
    let model = config
        .models
        .get(model_name)
        .with_context(|| format!("unknown model `{model_name}`"))?;
    let provider = config
        .providers
        .get(&model.provider)
        .with_context(|| format!("unknown provider `{}`", model.provider))?;
    let token = credential::get(&model.provider)?;

    let url = format!(
        "{}{}",
        provider.base_url.trim_end_matches('/'),
        provider.kind.upstream_path()
    );
    // v0.5 M5: the health check is timeout-bounded like every other
    // outbound call — an unresponsive gateway must not hang the probe.
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .context("cannot build the health check HTTP client")?;
    // The ping body shape is identical for both protocols; only the endpoint
    // and the anthropic-version header differ.
    let builder = client.post(url).json(&serde_json::json!({
        "model": model.model_id,
        "max_tokens": 1,
        "messages": [{"role": "user", "content": "ping"}]
    }));
    let builder = match provider.kind {
        ProviderKind::OpenAICompatible => builder,
        ProviderKind::Anthropic | ProviderKind::AnthropicCompatible => {
            builder.header("anthropic-version", "2023-06-01")
        }
    };
    let response = provider
        .apply_auth(builder, &token)
        .send()
        .await
        .context("provider request failed")?;

    let status = response.status();
    if status.is_success() {
        println!("healthy: {} / {}", model.provider, model.model_id);
        return Ok(());
    }

    if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
        anyhow::bail!("provider reachable but authentication failed ({status})");
    }

    let body = response.text().await.unwrap_or_default();
    // Invariant 1 hygiene (the discover.rs verify-pass fix, applied to
    // health's pre-existing twin): gateway error text is untrusted — a
    // debug-mode gateway can echo the just-sent credential back — so the
    // token is scrubbed before the body reaches the error message.
    anyhow::bail!(
        "provider returned {status}: {}",
        truncate(&body.replace(&token, "***"), 300)
    );
}

fn truncate(input: &str, max: usize) -> String {
    if input.chars().count() <= max {
        return input.to_string();
    }
    input.chars().take(max).collect::<String>() + "…"
}
