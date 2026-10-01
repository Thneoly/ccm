use anyhow::{Context, Result};
use reqwest::StatusCode;

use crate::{config::AppConfig, credential};

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

    let url = format!("{}/v1/messages", provider.base_url.trim_end_matches('/'));
    let client = reqwest::Client::new();
    let response = client
        .post(url)
        .header("x-api-key", &token)
        .header("authorization", format!("Bearer {token}"))
        .header("anthropic-version", "2023-06-01")
        .json(&serde_json::json!({
            "model": model.model_id,
            "max_tokens": 1,
            "messages": [{"role": "user", "content": "ping"}]
        }))
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

fn truncate(input: &str, max: usize) -> String {
    if input.chars().count() <= max {
        return input.to_string();
    }
    input.chars().take(max).collect::<String>() + "…"
}
