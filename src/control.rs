use anyhow::{Context, Result};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct SwitchResponse {
    current: String,
}

pub async fn switch(proxy_url: &str, target: &str) -> Result<()> {
    let url = format!(
        "{}/_ccm/switch/{}",
        proxy_url.trim_end_matches('/'),
        target
    );
    let response = reqwest::Client::new()
        .post(url)
        .send()
        .await
        .context("failed to contact CCM proxy control API")?;

    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        anyhow::bail!("CCM proxy rejected switch ({status}): {body}");
    }

    let body: SwitchResponse = response
        .json()
        .await
        .context("invalid CCM control response")?;
    println!("Runtime route switched to {}", body.current);
    Ok(())
}
