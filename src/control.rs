use anyhow::{anyhow, Context, Result};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct SwitchResponse {
    target: String,
}

pub async fn switch(proxy_url: &str, target: &str) -> Result<()> {
    let mut url = reqwest::Url::parse(proxy_url).context("invalid CCM proxy URL")?;
    {
        let mut segments = url
            .path_segments_mut()
            .map_err(|_| anyhow!("CCM proxy URL cannot be used as a base URL"))?;
        segments.pop_if_empty();
        segments.push("_ccm");
        segments.push("switch");
        segments.push(target);
    }

    let client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(3))
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .context("failed to build CCM control client")?;
    let response = client
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
    println!("Runtime target switched to {}", body.target);
    Ok(())
}
