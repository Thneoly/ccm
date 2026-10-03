use anyhow::{Context, Result};
use reqwest::StatusCode;

use crate::{
    config::AppConfig,
    credential,
    provider::{Provider, ProviderKind},
};

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
        health_path(provider)
    );
    let client = reqwest::Client::new();
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
    anyhow::bail!("provider returned {status}: {}", truncate(&body, 300));
}

/// Health-check endpoint for a provider kind.
fn health_path(provider: &Provider) -> &'static str {
    match provider.kind {
        ProviderKind::OpenAICompatible => "/v1/chat/completions",
        ProviderKind::Anthropic | ProviderKind::AnthropicCompatible => "/v1/messages",
    }
}

fn truncate(input: &str, max: usize) -> String {
    if input.chars().count() <= max {
        return input.to_string();
    }
    input.chars().take(max).collect::<String>() + "…"
}
