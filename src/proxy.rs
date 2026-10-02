use std::{
    cmp::Ordering,
    collections::{HashMap, VecDeque},
    net::SocketAddr,
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{bail, Context, Result};
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
use tokio::{
    sync::RwLock,
    time::{sleep, timeout},
};

use crate::{
    config::AppConfig,
    credential,
    route::{CircuitBreakerPolicy, RoutePolicy, SelectionStrategy, SelectionWeights},
};

const TRACE_CAPACITY: usize = 100;
const LATENCY_EWMA_ALPHA: f64 = 0.2;
const HEALTH_MIN_SAMPLES: u64 = 3;

#[derive(Clone)]
struct ProxyState {
    client: Client,
    target: Arc<RwLock<String>>,
    traces: Arc<RwLock<VecDeque<AttemptTrace>>>,
    circuits: Arc<RwLock<HashMap<String, CircuitState>>>,
    metrics: Arc<RwLock<HashMap<String, ModelMetrics>>>,
}

#[derive(Clone, Default)]
struct CircuitState {
    consecutive_failures: usize,
    open_until_ms: Option<u64>,
    half_open_probe_in_flight: bool,
}

#[derive(Clone, Copy)]
enum CircuitDecision {
    Closed,
    HalfOpen,
    SkipOpen(u64),
    SkipHalfOpen,
}

#[derive(Clone, Default)]
struct ModelMetrics {
    attempts: u64,
    successes: u64,
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
struct AttemptTrace {
    timestamp_ms: u64,
    target: String,
    attempt: usize,
    model: String,
    result: String,
    fallback: bool,
}

#[derive(Clone, Serialize)]
struct CircuitPolicyView {
    enabled: bool,
    failure_threshold: usize,
    open_ms: u64,
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
struct SelectionWeightsView {
    reliability: f64,
    latency: f64,
    cost: f64,
    quality: f64,
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
struct PolicyView {
    selection: String,
    weights: SelectionWeightsView,
    header_timeout_ms: u64,
    fallback_on: Vec<u16>,
    max_attempts: usize,
    backoff_ms: u64,
    circuit_breaker: CircuitPolicyView,
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
struct StatusView {
    target: String,
    primary: String,
    model_id: String,
    provider: String,
    fallback: Vec<String>,
    policy: PolicyView,
}

#[derive(Serialize)]
struct ModelView {
    name: String,
    model_id: String,
    provider: String,
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
        traces: Arc::new(RwLock::new(VecDeque::with_capacity(TRACE_CAPACITY))),
        circuits: Arc::new(RwLock::new(HashMap::new())),
        metrics: Arc::new(RwLock::new(HashMap::new())),
    };

    let app = Router::new()
        .route("/health", get(health))
        .route("/_ccm/status", get(control_status))
        .route("/_ccm/models", get(control_models))
        .route("/_ccm/routes", get(control_routes))
        .route("/_ccm/traces", get(control_traces))
        .route("/_ccm/circuits", get(control_circuits))
        .route("/_ccm/metrics", get(control_metrics))
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
        policy: PolicyView::from(&route.policy),
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
    let configured_candidates = route.candidates().map(str::to_string).collect::<Vec<_>>();
    let candidates = select_candidates(
        &state,
        &config,
        configured_candidates,
        &route.policy.selection,
        &route.policy.weights,
    )
    .await;

    let (parts, body) = request.into_parts();
    let bytes = axum::body::to_bytes(body, 16 * 1024 * 1024)
        .await
        .context("failed to read request body")?;

    let mut failures = Vec::new();
    let mut actual_attempts = 0usize;

    for (index, candidate) in candidates.iter().enumerate() {
        if actual_attempts >= route.policy.max_attempts {
            break;
        }

        match circuit_admit(&state, candidate, &route.policy.circuit_breaker).await {
            CircuitDecision::SkipOpen(until) => {
                let result = format!("skipped: circuit OPEN until {until}");
                trace_attempt(
                    &state,
                    &target,
                    actual_attempts + 1,
                    candidate,
                    &result,
                    index + 1 < candidates.len(),
                )
                .await;
                failures.push(format!("{candidate}: {result}"));
                continue;
            }
            CircuitDecision::SkipHalfOpen => {
                let result = "skipped: HALF_OPEN probe already in flight".to_string();
                trace_attempt(
                    &state,
                    &target,
                    actual_attempts + 1,
                    candidate,
                    &result,
                    index + 1 < candidates.len(),
                )
                .await;
                failures.push(format!("{candidate}: {result}"));
                continue;
            }
            CircuitDecision::Closed | CircuitDecision::HalfOpen => {}
        }

        actual_attempts += 1;
        let attempt = actual_attempts;
        let has_next_candidate = index + 1 < candidates.len();
        let has_attempt_budget = actual_attempts < route.policy.max_attempts;
        let can_fallback = has_next_candidate && has_attempt_budget;

        let model = config
            .models
            .get(candidate)
            .with_context(|| format!("route candidate model `{candidate}` is not configured"))?;
        let provider = config
            .providers
            .get(&model.provider)
            .with_context(|| format!("provider `{}` is not configured", model.provider))?;
        let token = credential::get(&model.provider)?;
        let body = rewrite_model(bytes.clone(), &model.model_id)?;
        let upstream = format!("{}/v1/messages", provider.base_url.trim_end_matches('/'));

        let mut builder = state.client.post(upstream).body(body);
        builder = copy_request_headers(builder, &parts.headers);
        builder = builder
            .header("x-api-key", &token)
            .header("authorization", format!("Bearer {token}"));

        let started = Instant::now();
        record_attempt_started(&state, candidate).await;
        let header_timeout = Duration::from_millis(route.policy.header_timeout_ms);
        let send = timeout(header_timeout, builder.send()).await;
        let upstream_response = match send {
            Err(_) => {
                let elapsed_ms = elapsed_ms(started);
                let result = format!("timeout after {}ms", route.policy.header_timeout_ms);
                record_timeout(&state, candidate, elapsed_ms).await;
                circuit_failure(&state, candidate, &route.policy.circuit_breaker).await;
                trace_attempt(&state, &target, attempt, candidate, &result, can_fallback).await;
                failures.push(format!("{candidate}: {result}"));
                if can_fallback {
                    apply_backoff(route.policy.backoff_ms).await;
                    continue;
                }
                break;
            }
            Ok(Err(err)) => {
                let elapsed_ms = elapsed_ms(started);
                let result = format!("request error: {err}");
                record_request_error(&state, candidate, elapsed_ms).await;
                circuit_failure(&state, candidate, &route.policy.circuit_breaker).await;
                trace_attempt(&state, &target, attempt, candidate, &result, can_fallback).await;
                failures.push(format!("{candidate}: {result}"));
                if can_fallback {
                    apply_backoff(route.policy.backoff_ms).await;
                    continue;
                }
                break;
            }
            Ok(Ok(response)) => {
                let elapsed_ms = elapsed_ms(started);
                record_http_response(
                    &state,
                    candidate,
                    response.status(),
                    elapsed_ms,
                    should_fallback_status(response.status(), &route.policy),
                )
                .await;
                response
            }
        };

        let status = upstream_response.status();
        if should_fallback_status(status, &route.policy) {
            circuit_failure(&state, candidate, &route.policy.circuit_breaker).await;
            let result = format!("HTTP {status}");
            trace_attempt(&state, &target, attempt, candidate, &result, can_fallback).await;
            failures.push(format!("{candidate}: {result}"));
            if can_fallback {
                apply_backoff(route.policy.backoff_ms).await;
                continue;
            }
            return proxy_response(upstream_response);
        }

        circuit_success(&state, candidate, &route.policy.circuit_breaker).await;
        trace_attempt(
            &state,
            &target,
            attempt,
            candidate,
            &format!("HTTP {status}"),
            false,
        )
        .await;
        return proxy_response(upstream_response);
    }

    bail!(
        "all available route candidates failed for `{target}`: {}",
        failures.join("; ")
    )
}

async fn select_candidates(
    state: &ProxyState,
    config: &AppConfig,
    mut candidates: Vec<String>,
    selection: &SelectionStrategy,
    weights: &SelectionWeights,
) -> Vec<String> {
    match selection {
        SelectionStrategy::Ordered => candidates,
        SelectionStrategy::Healthiest => {
            let metrics = state.metrics.read().await;
            candidates.sort_by(|left, right| {
                let left_score = candidate_health_rank(metrics.get(left));
                let right_score = candidate_health_rank(metrics.get(right));
                right_score
                    .partial_cmp(&left_score)
                    .unwrap_or(Ordering::Equal)
                    .then_with(|| compare_latency(metrics.get(left), metrics.get(right)))
            });
            candidates
        }
        SelectionStrategy::LowestLatency => {
            let metrics = state.metrics.read().await;
            candidates.sort_by(|left, right| compare_latency(metrics.get(left), metrics.get(right)));
            candidates
        }
        SelectionStrategy::LowestCost => {
            candidates.sort_by(|left, right| {
                model_cost(config, left)
                    .partial_cmp(&model_cost(config, right))
                    .unwrap_or(Ordering::Equal)
            });
            candidates
        }
        SelectionStrategy::Weighted => {
            let metrics = state.metrics.read().await;
            candidates.sort_by(|left, right| {
                let left_score = weighted_score(config, metrics.get(left), left, weights);
                let right_score = weighted_score(config, metrics.get(right), right, weights);
                right_score
                    .partial_cmp(&left_score)
                    .unwrap_or(Ordering::Equal)
            });
            candidates
        }
    }
}

fn candidate_health_rank(metrics: Option<&ModelMetrics>) -> f64 {
    match metrics {
        Some(metrics) if metrics.attempts >= HEALTH_MIN_SAMPLES => success_rate(metrics),
        _ => 1.0,
    }
}

fn weighted_score(
    config: &AppConfig,
    metrics: Option<&ModelMetrics>,
    model: &str,
    weights: &SelectionWeights,
) -> f64 {
    let reliability = candidate_health_rank(metrics);
    let latency = latency_score(metrics);
    let cost = 1.0 / (1.0 + model_cost(config, model).max(0.0));
    let quality = model_quality(config, model).clamp(0.0, 1.0);
    let total = weights.reliability + weights.latency + weights.cost + weights.quality;

    if total <= 0.0 {
        return 0.0;
    }

    (weights.reliability * reliability
        + weights.latency * latency
        + weights.cost * cost
        + weights.quality * quality)
        / total
}

fn latency_score(metrics: Option<&ModelMetrics>) -> f64 {
    match metrics.and_then(|metrics| metrics.latency_ewma_ms) {
        Some(latency_ms) => 1.0 / (1.0 + latency_ms.max(0.0) / 1000.0),
        None => 0.5,
    }
}

fn model_cost(config: &AppConfig, model: &str) -> f64 {
    config
        .models
        .get(model)
        .map(|model| model.routing.cost_weight)
        .unwrap_or(1.0)
}

fn model_quality(config: &AppConfig, model: &str) -> f64 {
    config
        .models
        .get(model)
        .map(|model| model.routing.quality_weight)
        .unwrap_or(1.0)
}

fn compare_latency(left: Option<&ModelMetrics>, right: Option<&ModelMetrics>) -> Ordering {
    match (
        left.and_then(|metrics| metrics.latency_ewma_ms),
        right.and_then(|metrics| metrics.latency_ewma_ms),
    ) {
        (Some(left), Some(right)) => left.partial_cmp(&right).unwrap_or(Ordering::Equal),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}

async fn circuit_admit(
    state: &ProxyState,
    model: &str,
    policy: &CircuitBreakerPolicy,
) -> CircuitDecision {
    if !policy.enabled {
        return CircuitDecision::Closed;
    }

    let now = now_ms();
    let mut circuits = state.circuits.write().await;
    let circuit = circuits.entry(model.to_string()).or_default();

    match circuit.open_until_ms {
        Some(until) if until > now => CircuitDecision::SkipOpen(until),
        Some(_) if circuit.half_open_probe_in_flight => CircuitDecision::SkipHalfOpen,
        Some(_) => {
            circuit.half_open_probe_in_flight = true;
            CircuitDecision::HalfOpen
        }
        None => CircuitDecision::Closed,
    }
}

async fn circuit_failure(
    state: &ProxyState,
    model: &str,
    policy: &CircuitBreakerPolicy,
) {
    if !policy.enabled {
        return;
    }

    let mut circuits = state.circuits.write().await;
    let circuit = circuits.entry(model.to_string()).or_default();
    circuit.consecutive_failures += 1;

    if circuit.half_open_probe_in_flight
        || circuit.consecutive_failures >= policy.failure_threshold
    {
        circuit.open_until_ms = Some(now_ms().saturating_add(policy.open_ms));
        circuit.half_open_probe_in_flight = false;
    }
}

async fn circuit_success(
    state: &ProxyState,
    model: &str,
    policy: &CircuitBreakerPolicy,
) {
    if !policy.enabled {
        return;
    }

    let mut circuits = state.circuits.write().await;
    let circuit = circuits.entry(model.to_string()).or_default();
    circuit.consecutive_failures = 0;
    circuit.open_until_ms = None;
    circuit.half_open_probe_in_flight = false;
}

async fn record_attempt_started(state: &ProxyState, model: &str) {
    let mut metrics = state.metrics.write().await;
    metrics.entry(model.to_string()).or_default().attempts += 1;
}

async fn record_timeout(state: &ProxyState, model: &str, latency_ms: f64) {
    let mut metrics = state.metrics.write().await;
    let metric = metrics.entry(model.to_string()).or_default();
    metric.timeouts += 1;
    metric.last_failure_ms = Some(now_ms());
    update_latency_ewma(metric, latency_ms);
}

async fn record_request_error(state: &ProxyState, model: &str, latency_ms: f64) {
    let mut metrics = state.metrics.write().await;
    let metric = metrics.entry(model.to_string()).or_default();
    metric.request_errors += 1;
    metric.last_failure_ms = Some(now_ms());
    update_latency_ewma(metric, latency_ms);
}

async fn record_http_response(
    state: &ProxyState,
    model: &str,
    status: reqwest::StatusCode,
    latency_ms: f64,
    fallback_failure: bool,
) {
    let mut metrics = state.metrics.write().await;
    let metric = metrics.entry(model.to_string()).or_default();
    update_latency_ewma(metric, latency_ms);

    if status.is_success() {
        metric.successes += 1;
        metric.last_success_ms = Some(now_ms());
    } else {
        metric.http_errors += 1;
        metric.last_failure_ms = Some(now_ms());
    }

    if fallback_failure {
        metric.fallback_failures += 1;
    }
    if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
        metric.rate_limited += 1;
    }
}

fn update_latency_ewma(metrics: &mut ModelMetrics, latency_ms: f64) {
    metrics.latency_ewma_ms = Some(match metrics.latency_ewma_ms {
        Some(previous) => {
            LATENCY_EWMA_ALPHA * latency_ms + (1.0 - LATENCY_EWMA_ALPHA) * previous
        }
        None => latency_ms,
    });
}

fn success_rate(metrics: &ModelMetrics) -> f64 {
    if metrics.attempts == 0 {
        0.0
    } else {
        metrics.successes as f64 / metrics.attempts as f64
    }
}

fn health_score(metrics: &ModelMetrics) -> Option<f64> {
    if metrics.attempts == 0 {
        None
    } else {
        Some(success_rate(metrics) * 100.0)
    }
}

fn elapsed_ms(started: Instant) -> f64 {
    started.elapsed().as_secs_f64() * 1000.0
}

fn selection_name(selection: &SelectionStrategy) -> &'static str {
    match selection {
        SelectionStrategy::Ordered => "ordered",
        SelectionStrategy::Healthiest => "healthiest",
        SelectionStrategy::LowestLatency => "lowest-latency",
        SelectionStrategy::LowestCost => "lowest-cost",
        SelectionStrategy::Weighted => "weighted",
    }
}

async fn apply_backoff(backoff_ms: u64) {
    if backoff_ms > 0 {
        sleep(Duration::from_millis(backoff_ms)).await;
    }
}

async fn trace_attempt(
    state: &ProxyState,
    target: &str,
    attempt: usize,
    model: &str,
    result: &str,
    fallback: bool,
) {
    let trace = AttemptTrace {
        timestamp_ms: now_ms(),
        target: target.to_string(),
        attempt,
        model: model.to_string(),
        result: result.to_string(),
        fallback,
    };

    if fallback {
        eprintln!(
            "ccm route={target} attempt={attempt} model={model} result={result} action=fallback"
        );
    } else {
        eprintln!("ccm route={target} attempt={attempt} model={model} result={result}");
    }

    let mut traces = state.traces.write().await;
    if traces.len() == TRACE_CAPACITY {
        traces.pop_front();
    }
    traces.push_back(trace);
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64
}

fn should_fallback_status(status: reqwest::StatusCode, policy: &RoutePolicy) -> bool {
    policy.fallback_on.contains(&status.as_u16())
}

fn proxy_response(upstream_response: reqwest::Response) -> Result<Response<Body>> {
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
        assert_eq!(status.policy.header_timeout_ms, 30_000);
        assert_eq!(status.policy.selection, "ordered");
        assert_eq!(status.policy.weights.reliability, 0.4);
        assert!(status.policy.circuit_breaker.enabled);
    }

    #[test]
    fn fallback_status_comes_from_policy() {
        let policy = RoutePolicy {
            fallback_on: vec![429, 503],
            ..RoutePolicy::default()
        };
        assert!(should_fallback_status(
            reqwest::StatusCode::TOO_MANY_REQUESTS,
            &policy
        ));
        assert!(should_fallback_status(
            reqwest::StatusCode::SERVICE_UNAVAILABLE,
            &policy
        ));
        assert!(!should_fallback_status(
            reqwest::StatusCode::BAD_GATEWAY,
            &policy
        ));
    }

    #[test]
    fn latency_ewma_tracks_recent_samples() {
        let mut metrics = ModelMetrics::default();
        update_latency_ewma(&mut metrics, 100.0);
        update_latency_ewma(&mut metrics, 200.0);
        assert_eq!(metrics.latency_ewma_ms, Some(120.0));
    }

    #[test]
    fn success_rate_uses_real_attempts() {
        let metrics = ModelMetrics {
            attempts: 4,
            successes: 3,
            ..ModelMetrics::default()
        };
        assert_eq!(success_rate(&metrics), 0.75);
        assert_eq!(health_score(&metrics), Some(75.0));
    }

    #[test]
    fn weighted_score_uses_model_metadata() {
        let config = AppConfig::starter();
        let weights = SelectionWeights::default();
        let score = weighted_score(&config, None, "glm", &weights);
        assert!(score > 0.0);
        assert!(score < 1.0);
    }

    #[test]
    fn healthiest_waits_for_minimum_samples() {
        let metrics = ModelMetrics {
            attempts: 2,
            successes: 0,
            ..ModelMetrics::default()
        };
        assert_eq!(candidate_health_rank(Some(&metrics)), 1.0);
    }
}
