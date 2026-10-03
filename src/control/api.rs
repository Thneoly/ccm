//! The proxy's control-plane HTTP API: `/_ccm` view structs, handlers, and
//! Router construction. Split out of `src/proxy.rs` (v0.4 M0).

use std::cmp::Ordering;

use anyhow::{Context, Result};
use axum::{
    body::Body,
    extract::{Path, State},
    http::{Response, StatusCode},
    response::IntoResponse,
    routing::{any, get, post},
    Json, Router,
};
use serde::Serialize;

use crate::{
    config::AppConfig,
    proxy::{forward_messages, ProxyState},
    route::{CircuitBreakerPolicy, RoutePolicy, SelectionWeights},
    routing::decision::{now_ms, AttemptTrace, RoutingDecision},
    routing::metrics::{health_score, success_rate},
    routing::select::{candidate_score_view, selection_name},
};

#[derive(Clone, Serialize)]
struct ModelMetricsView {
    model: String,
    attempts: u64,
    successes: u64,
    success_rate: f64,
    health_score: Option<f64>,
    http_errors: u64,
    fallback_failures: u64,
    timeouts: u64,
    request_errors: u64,
    rate_limited: u64,
    latency_ewma_ms: Option<f64>,
    last_success_ms: Option<u64>,
    last_failure_ms: Option<u64>,
}

#[derive(Clone, Serialize)]
pub(crate) struct CircuitPolicyView {
    pub(crate) enabled: bool,
    pub(crate) failure_threshold: usize,
    pub(crate) open_ms: u64,
}

impl From<&CircuitBreakerPolicy> for CircuitPolicyView {
    fn from(policy: &CircuitBreakerPolicy) -> Self {
        Self {
            enabled: policy.enabled,
            failure_threshold: policy.failure_threshold,
            open_ms: policy.open_ms,
        }
    }
}

#[derive(Clone, Serialize)]
pub(crate) struct SelectionWeightsView {
    pub(crate) reliability: f64,
    pub(crate) latency: f64,
    pub(crate) cost: f64,
    pub(crate) quality: f64,
}

impl From<&SelectionWeights> for SelectionWeightsView {
    fn from(weights: &SelectionWeights) -> Self {
        Self {
            reliability: weights.reliability,
            latency: weights.latency,
            cost: weights.cost,
            quality: weights.quality,
        }
    }
}

#[derive(Clone, Serialize)]
pub(crate) struct PolicyView {
    pub(crate) selection: String,
    pub(crate) weights: SelectionWeightsView,
    pub(crate) header_timeout_ms: u64,
    pub(crate) fallback_on: Vec<u16>,
    pub(crate) max_attempts: usize,
    pub(crate) backoff_ms: u64,
    pub(crate) circuit_breaker: CircuitPolicyView,
}

impl From<&RoutePolicy> for PolicyView {
    fn from(policy: &RoutePolicy) -> Self {
        Self {
            selection: selection_name(&policy.selection).to_string(),
            weights: SelectionWeightsView::from(&policy.weights),
            header_timeout_ms: policy.header_timeout_ms,
            fallback_on: policy.fallback_on.clone(),
            max_attempts: policy.max_attempts,
            backoff_ms: policy.backoff_ms,
            circuit_breaker: CircuitPolicyView::from(&policy.circuit_breaker),
        }
    }
}

#[derive(Serialize)]
pub(crate) struct StatusView {
    pub(crate) target: String,
    pub(crate) primary: String,
    pub(crate) model_id: String,
    pub(crate) provider: String,
    pub(crate) kind: String,
    pub(crate) fallback: Vec<String>,
    pub(crate) policy: PolicyView,
}

#[derive(Serialize)]
struct ModelView {
    name: String,
    model_id: String,
    provider: String,
    kind: String,
    cost_weight: f64,
    quality_weight: f64,
}

#[derive(Serialize)]
struct RouteView {
    name: String,
    primary: String,
    fallback: Vec<String>,
    policy: PolicyView,
    active: bool,
}

#[derive(Serialize)]
struct SwitchView {
    requested: String,
    target: String,
    primary: String,
    fallback: Vec<String>,
    policy: PolicyView,
}

#[derive(Serialize)]
struct CircuitView {
    model: String,
    state: String,
    consecutive_failures: usize,
    open_until_ms: Option<u64>,
}

pub(crate) fn control_router(state: ProxyState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/_ccm/status", get(control_status))
        .route("/_ccm/models", get(control_models))
        .route("/_ccm/routes", get(control_routes))
        .route("/_ccm/traces", get(control_traces))
        .route("/_ccm/circuits", get(control_circuits))
        .route("/_ccm/metrics", get(control_metrics))
        .route("/_ccm/scores", get(control_scores))
        .route("/_ccm/decisions", get(control_decisions))
        .route("/_ccm/switch/{target}", post(control_switch))
        .route("/v1/messages", any(forward_messages))
        .with_state(state)
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
                    kind: provider_kind_name(&config, &model.provider),
                    cost_weight: model.routing.cost_weight,
                    quality_weight: model.routing.quality_weight,
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
                    policy: PolicyView::from(&route.policy),
                    active: name == &active,
                })
                .collect::<Vec<_>>();
            Json(routes).into_response()
        }
        Err(err) => control_error(StatusCode::INTERNAL_SERVER_ERROR, err),
    }
}

async fn control_traces(State(state): State<ProxyState>) -> Json<Vec<AttemptTrace>> {
    let traces = state.traces.read().await.iter().cloned().collect();
    Json(traces)
}

async fn control_circuits(State(state): State<ProxyState>) -> Json<Vec<CircuitView>> {
    let now = now_ms();
    let circuits = state.circuits.read().await;
    let mut views = circuits
        .iter()
        .map(|(model, circuit)| {
            let state_name = match circuit.open_until_ms {
                Some(until) if until > now => "OPEN",
                Some(_) if circuit.half_open_probe_in_flight => "HALF_OPEN",
                Some(_) => "HALF_OPEN_READY",
                None => "CLOSED",
            };
            CircuitView {
                model: model.clone(),
                state: state_name.to_string(),
                consecutive_failures: circuit.consecutive_failures,
                open_until_ms: circuit.open_until_ms,
            }
        })
        .collect::<Vec<_>>();
    views.sort_by(|left, right| left.model.cmp(&right.model));
    Json(views)
}

async fn control_metrics(State(state): State<ProxyState>) -> Json<Vec<ModelMetricsView>> {
    let metrics = state.metrics.read().await;
    let mut views = metrics
        .iter()
        .map(|(model, metrics)| ModelMetricsView {
            model: model.clone(),
            attempts: metrics.attempts,
            successes: metrics.successes,
            success_rate: success_rate(metrics),
            health_score: health_score(metrics),
            http_errors: metrics.http_errors,
            fallback_failures: metrics.fallback_failures,
            timeouts: metrics.timeouts,
            request_errors: metrics.request_errors,
            rate_limited: metrics.rate_limited,
            latency_ewma_ms: metrics.latency_ewma_ms,
            last_success_ms: metrics.last_success_ms,
            last_failure_ms: metrics.last_failure_ms,
        })
        .collect::<Vec<_>>();
    views.sort_by(|left, right| left.model.cmp(&right.model));
    Json(views)
}

async fn control_scores(State(state): State<ProxyState>) -> impl IntoResponse {
    let target = state.target.read().await.clone();
    let config = match AppConfig::load() {
        Ok(config) => config,
        Err(err) => return control_error(StatusCode::INTERNAL_SERVER_ERROR, err),
    };
    let route = match config.resolve_route(&target) {
        Ok(route) => route,
        Err(err) => return control_error(StatusCode::INTERNAL_SERVER_ERROR, err),
    };
    let metrics = state.metrics.read().await;
    let mut views = route
        .candidates()
        .map(|model| {
            candidate_score_view(&config, metrics.get(model), model, &route.policy.weights)
        })
        .collect::<Vec<_>>();
    views.sort_by(|left, right| {
        right
            .weighted_score
            .partial_cmp(&left.weighted_score)
            .unwrap_or(Ordering::Equal)
    });
    Json(views).into_response()
}

async fn control_decisions(State(state): State<ProxyState>) -> Json<Vec<RoutingDecision>> {
    let decisions = state.decisions.read().await.iter().cloned().collect();
    Json(decisions)
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
                policy: PolicyView::from(&route.policy),
            })
            .into_response()
        }
        Err(err) => control_error(StatusCode::BAD_REQUEST, err),
    }
}

fn control_error(status: StatusCode, err: anyhow::Error) -> Response<Body> {
    (status, format!("CCM control error: {err:#}")).into_response()
}

pub(crate) fn status_view(config: &AppConfig, target: &str) -> Result<StatusView> {
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
        kind: provider_kind_name(config, &model.provider),
        fallback: route.fallback,
        policy: PolicyView::from(&route.policy),
    })
}

/// Provider kind name for the JSON views; `unknown` keeps the view additive
/// when a model references a provider that is not configured.
fn provider_kind_name(config: &AppConfig, provider_name: &str) -> String {
    config
        .providers
        .get(provider_name)
        .map(|provider| provider.kind.as_str().to_string())
        .unwrap_or_else(|| "unknown".to_string())
}
