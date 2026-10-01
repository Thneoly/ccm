use std::{net::SocketAddr, sync::Arc};

use anyhow::{Context, Result};
use axum::{
    body::{Body, Bytes},
    extract::{Path, State},
    http::{header, HeaderMap, Method, Request, Response, StatusCode},
    response::IntoResponse,
    routing::{any, get, post},
    Json, Router,
};
use reqwest::Client;
use serde::Serialize;
use serde_json::Value;
use tokio::sync::RwLock;

use crate::{config::AppConfig, credential};

#[derive(Clone)]
struct ProxyState {
    client: Client,
    target: Arc<RwLock<String>>,
}

#[derive(Serialize)]
struct StatusView {
    target: String,
    primary: String,
    model_id: String,
    provider: String,
    fallback: Vec<String>,
}

#[derive(Serialize)]
struct ModelView {
    name: String,
    model_id: String,
    provider: String,
}

#[derive(Serialize)]
struct RouteView {
    name: String,
    primary: String,
    fallback: Vec<String>,
    active: bool,
}

#[derive(Serialize)]
struct SwitchView {
    requested: String,
    target: String,
    primary: String,
    fallback: Vec<String>,
}

pub async fn serve(bind: &str) -> Result<()> {
    let addr: SocketAddr = bind
        .parse()
        .with_context(|| format!("invalid bind address `{bind}`"))?;

    let config = AppConfig::load().context("failed to load CCM config")?;
    let target = config
        .current
        .clone()
        .context("no current target selected; run `ccm use <name>` first")?;
    config.resolve_route(&target)?;

    let state = ProxyState {
        client: Client::new(),
        target: Arc::new(RwLock::new(target)),
    };

    let app = Router::new()
        .route("/health", get(health))
        .route("/_ccm/status", get(control_status))
        .route("/_ccm/models", get(control_models))
        .route("/_ccm/routes", get(control_routes))
        .route("/_ccm/switch/{target}", post(control_switch))
        .route("/v1/messages", any(forward_messages))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("failed to bind {addr}"))?;

    println!("CCM proxy listening on http://{addr}");
    println!("Claude Code base URL: http://{addr}");
    println!("Runtime switch: `ccm switch <model-or-profile-or-route>`.");

    axum::serve(listener, app).await.context("proxy server failed")
}

async fn health() -> &'static str {
    "ok"
}

async fn control_status(State(state): State<ProxyState>) -> impl IntoResponse {
    let target = state.target.read().await.clone();
    match AppConfig::load().and_then(|config| status_view(&config, &target)) {
        Ok(view) => Json(view).into_response(),
        Err(err) => control_error(StatusCode::INTERNAL_SERVER_ERROR, err),
    }
}

async fn control_models() -> impl IntoResponse {
    match AppConfig::load() {
        Ok(config) => {
            let models = config
                .models
                .iter()
                .map(|(name, model)| ModelView {
                    name: name.clone(),
                    model_id: model.model_id.clone(),
                    provider: model.provider.clone(),
                })
                .collect::<Vec<_>>();
            Json(models).into_response()
        }
        Err(err) => control_error(StatusCode::INTERNAL_SERVER_ERROR, err),
    }
}

async fn control_routes(State(state): State<ProxyState>) -> impl IntoResponse {
    let active = state.target.read().await.clone();
    match AppConfig::load() {
        Ok(config) => {
            let routes = config
                .routes
                .iter()
                .map(|(name, route)| RouteView {
                    name: name.clone(),
                    primary: route.primary.clone(),
                    fallback: route.fallback.clone(),
                    active: name == &active,
                })
                .collect::<Vec<_>>();
            Json(routes).into_response()
        }
        Err(err) => control_error(StatusCode::INTERNAL_SERVER_ERROR, err),
    }
}

async fn control_switch(
    State(state): State<ProxyState>,
    Path(target): Path<String>,
) -> impl IntoResponse {
    match AppConfig::load().and_then(|config| config.resolve_route(&target)) {
        Ok(route) => {
            *state.target.write().await = route.target.clone();
            Json(SwitchView {
                requested: target,
                target: route.target,
                primary: route.primary,
                fallback: route.fallback,
            })
            .into_response()
        }
        Err(err) => control_error(StatusCode::BAD_REQUEST, err),
    }
}

fn control_error(status: StatusCode, err: anyhow::Error) -> Response<Body> {
    (status, format!("CCM control error: {err:#}")).into_response()
}

fn status_view(config: &AppConfig, target: &str) -> Result<StatusView> {
    let route = config.resolve_route(target)?;
    let model = config
        .models
        .get(&route.primary)
        .with_context(|| format!("primary model `{}` is not configured", route.primary))?;
    Ok(StatusView {
        target: route.target,
        primary: route.primary,
        model_id: model.model_id.clone(),
        provider: model.provider.clone(),
        fallback: route.fallback,
    })
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

    let target = state.target.read().await.clone();
    let config = AppConfig::load().context("failed to reload CCM config")?;
    let route = config.resolve_route(&target)?;
    let model = config
        .models
        .get(&route.primary)
        .with_context(|| format!("primary model `{}` is not configured", route.primary))?;
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

fn copy_request_headers(
    builder: reqwest::RequestBuilder,
    headers: &HeaderMap,
) -> reqwest::RequestBuilder {
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

    #[test]
    fn builds_route_status_view() {
        let config = AppConfig::starter();
        let status = status_view(&config, "coding-route").unwrap();
        assert_eq!(status.target, "coding-route");
        assert_eq!(status.primary, "claude");
        assert_eq!(status.fallback, vec!["glm"]);
    }
}
