//! Runtime control plane. `api` hosts the proxy's `/_ccm` HTTP handlers;
//! this module's `switch` / `clients` are the CLI-side clients for
//! `ccm switch` / `ccm clients`.

pub(crate) mod api;

use anyhow::{anyhow, Context, Result};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct SwitchResponse {
    target: String,
}

/// One `/_ccm/clients` row as printed by `ccm clients`. Local struct on
/// purpose — the CLI stays decoupled from the api view types (extra
/// response fields are ignored).
#[derive(Debug, Deserialize)]
struct ClientRow {
    client: String,
    target: String,
    requests: u64,
}

/// Switch the proxy's runtime target. `client = None` moves the global
/// target; `client = Some(id)` scopes the switch to that client only
/// (`POST /_ccm/switch/{target}?client=<id>`).
pub async fn switch(proxy_url: &str, target: &str, client: Option<&str>) -> Result<()> {
    let url = switch_url(proxy_url, target, client)?;
    let response = control_client()?
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
    match client {
        Some(id) => println!("Runtime target for client {id} switched to {}", body.target),
        None => println!("Runtime target switched to {}", body.target),
    }
    Ok(())
}

/// List the proxy's per-client runtime entries (`GET /_ccm/clients`).
pub async fn clients(proxy_url: &str) -> Result<()> {
    let url = control_url(proxy_url, &["_ccm", "clients"])?;
    let response = control_client()?
        .get(url)
        .send()
        .await
        .context("failed to contact CCM proxy control API")?;

    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        anyhow::bail!("CCM proxy rejected clients listing ({status}): {body}");
    }

    let rows: Vec<ClientRow> = response
        .json()
        .await
        .context("invalid CCM control response")?;
    if rows.is_empty() {
        println!("No clients with a scoped target (one appears after `ccm switch <target> --client <id>` or `ccm run --proxy <target> --client <id>`)");
        return Ok(());
    }
    for row in rows {
        println!(
            "{:16} {:16} requests={}",
            row.client, row.target, row.requests
        );
    }
    Ok(())
}

fn control_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(3))
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .context("failed to build CCM control client")
}

/// Build a control URL by appending path segments to the proxy base. The
/// `url` crate does all encoding — segments are never hand-concatenated.
fn control_url(proxy_url: &str, segments: &[&str]) -> Result<reqwest::Url> {
    let mut url = reqwest::Url::parse(proxy_url).context("invalid CCM proxy URL")?;
    {
        let mut path = url
            .path_segments_mut()
            .map_err(|_| anyhow!("CCM proxy URL cannot be used as a base URL"))?;
        path.pop_if_empty();
        for segment in segments {
            path.push(segment);
        }
    }
    Ok(url)
}

fn switch_url(proxy_url: &str, target: &str, client: Option<&str>) -> Result<reqwest::Url> {
    let mut url = control_url(proxy_url, &["_ccm", "switch", target])?;
    if let Some(id) = client {
        url.query_pairs_mut().append_pair("client", id);
    }
    Ok(url)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn switch_url_without_client_has_no_query() {
        let url = switch_url("http://127.0.0.1:13521", "glm", None).unwrap();
        assert_eq!(url.as_str(), "http://127.0.0.1:13521/_ccm/switch/glm");
    }

    #[test]
    fn switch_url_appends_client_query_pair() {
        let url = switch_url("http://127.0.0.1:13521/", "glm", Some("a.b-1")).unwrap();
        assert_eq!(
            url.as_str(),
            "http://127.0.0.1:13521/_ccm/switch/glm?client=a.b-1"
        );
    }

    #[test]
    fn clients_url_uses_the_clients_path() {
        let url = control_url("http://127.0.0.1:13521", &["_ccm", "clients"]).unwrap();
        assert_eq!(url.as_str(), "http://127.0.0.1:13521/_ccm/clients");
    }
}
