use std::net::SocketAddr;

use anyhow::{Context, Result};
use axum::{
    body::{Body, Bytes},
    extract::State,
    http::{header, HeaderMap, Method, Request, Response, StatusCode},
    response::IntoResponse,
    routing::any,
    Router,
};
use reqwest::Client;
use serde_json::Value;

use crate::{config::AppConfig, credential};

#[derive(Clone)]
struct ProxyState {
    client: Client,
}

pub async fn serve(bind: &str) -> Result<()> {
    let addr: SocketAddr = bind
        .parse()
        .with_context(|| format!("invalid bind address `{bind}`"))?;

    let state = ProxyState {
        client: Client::new(),
    };

    let app = Router::new()
        .route("/health", any(health))
        .route("/v1/messages", any(forward_messages))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("failed to bind {addr}"))?;

    println!("CCM proxy listening on http://{addr}");
    println!("Claude Code base URL: http://{addr}");
    println!("Switch active backend with `ccm use <model-or-profile>`.");

    axum::serve(listener, app).await.context("proxy server failed")
}

async fn health() -> &'static str {
    "ok"
}

async fn forward_messages(
    State(state): State<ProxyState>,
    request: Request<Body>,
) -> impl IntoResponse {
    match forward(state, request).await {
        Ok(response) => response,
        Err(err) => (
            StatusCode::BAD_GATEWAY,
            format!("CCM proxy error: {err:#}"),
        )
            .into_response(),
    }
}

async fn forward(state: ProxyState, request: Request<Body>) -> Result<Response<Body>> {
    if request.method() != Method::POST {
        return Ok((StatusCode::METHOD_NOT_ALLOWED, "POST required").into_response());
    }

    let config = AppConfig::load().context("failed to reload CCM config")?;
    let current = config
        .current
        .clone()
        .context("no current model selected; run `ccm use <name>` first")?;
    let model = config
        .models
        .get(&current)
        .with_context(|| format!("current model `{current}` is not configured"))?;
    let provider = config
        .providers
        .get(&model.provider)
        .with_context(|| format!("provider `{}` is not configured", model.provider))?;
    let token = credential::get(&model.provider)?;

    let (parts, body) = request.into_parts();
    let bytes = axum::body::to_bytes(body, 16 * 1024 * 1024)
        .await
        .context("failed to read request body")?;
    let body = rewrite_model(bytes, &model.model_id)?;

    let upstream = format!("{}/v1/messages", provider.base_url.trim_end_matches('/'));
    let mut builder = state.client.post(upstream).body(body);
    builder = copy_request_headers(builder, &parts.headers);
    builder = builder
        .header("x-api-key", &token)
        .header("authorization", format!("Bearer {token}"));

    let upstream_response = builder.send().await.context("upstream request failed")?;
    let status = upstream_response.status();
    let headers = upstream_response.headers().clone();
    let stream = upstream_response.bytes_stream();

    let mut response = Response::builder().status(status.as_u16());
    for (name, value) in headers.iter() {
        if is_hop_by_hop(name.as_str()) || name == header::CONTENT_LENGTH {
            continue;
        }
        response = response.header(name, value);
    }

    response
        .body(Body::from_stream(stream))
        .context("failed to build proxy response")
}

fn rewrite_model(bytes: Bytes, model_id: &str) -> Result<Vec<u8>> {
    let mut value: Value = serde_json::from_slice(&bytes).context("request body is not valid JSON")?;
    let object = value
        .as_object_mut()
        .context("request body must be a JSON object")?;
    object.insert("model".to_string(), Value::String(model_id.to_string()));
    serde_json::to_vec(&value).context("failed to serialize request body")
}

fn copy_request_headers(builder: reqwest::RequestBuilder, headers: &HeaderMap) -> reqwest::RequestBuilder {
    headers.iter().fold(builder, |builder, (name, value)| {
        if is_hop_by_hop(name.as_str())
            || name == header::HOST
            || name == header::CONTENT_LENGTH
            || name.as_str().eq_ignore_ascii_case("x-api-key")
            || name == header::AUTHORIZATION
        {
            builder
        } else {
            builder.header(name, value)
        }
    })
}

fn is_hop_by_hop(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewrites_model_field() {
        let input = Bytes::from_static(br#"{"model":"old","messages":[]}"#);
        let output = rewrite_model(input, "new-model").unwrap();
        let value: Value = serde_json::from_slice(&output).unwrap();
        assert_eq!(value["model"], "new-model");
    }
}
