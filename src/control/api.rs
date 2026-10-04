//! The proxy's control-plane HTTP API: `/_ccm` view structs, handlers, and
//! Router construction. Split out of `src/proxy.rs` (v0.4 M0).

use std::cmp::Ordering;

use anyhow::{bail, Context, Result};
use axum::{
    body::Body,
    extract::{Path, Query, State},
    http::{Response, StatusCode},
    response::IntoResponse,
    routing::{any, get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};

use crate::{
    config::AppConfig,
    proxy::{
        forward_messages, invalid_client_id_message, valid_client_id, ClientEntry, ProxyState,
    },
    route::{CircuitBreakerPolicy, RoutePolicy, SelectionWeights},
    routing::decision::{now_ms, AttemptTrace, RoutingDecision},
    routing::metrics::{health_score, success_rate, ModelMetrics},
    routing::select::{candidate_score_view, selection_name},
};

/// `?client=<id>` query parameter shared by the client-aware endpoints.
#[derive(Debug, Default, Deserialize)]
pub(crate) struct ClientParams {
    pub(crate) client: Option<String>,
}

/// `/_ccm/decisions` query (v0.4 M5): `?client=` as before, plus disk-backed
/// history filters `?since=&until=&model=` and `?limit=`. Without any of
/// since/until/model the handler serves the in-memory ring (last 100
/// decisions); `?limit=` applies on that branch too, truncating to the most
/// recent N. Disk reads keep the most recent `limit` records and the read
/// itself is bounded — files are walked newest-first and parsing stops once
/// `limit` matches are held (default 1000 when `limit` is absent).
#[derive(Debug, Default, Deserialize)]
pub(crate) struct DecisionParams {
    pub(crate) client: Option<String>,
    pub(crate) since: Option<u64>,
    pub(crate) until: Option<u64>,
    pub(crate) model: Option<String>,
    pub(crate) limit: Option<usize>,
}

/// `/_ccm/usage` query (v0.4 M6): the same dual-branch contract as
/// `/_ccm/decisions` — without any of since/until/model the in-memory ring
/// (last 100 records) is served; any of them switches to a bounded disk
/// read over the persisted usage.jsonl. `?limit=` applies on both branches.
#[derive(Debug, Default, Deserialize)]
pub(crate) struct UsageParams {
    pub(crate) client: Option<String>,
    pub(crate) since: Option<u64>,
    pub(crate) until: Option<u64>,
    pub(crate) model: Option<String>,
    pub(crate) limit: Option<usize>,
}

/// `/_ccm/cost` query (v0.4 M6): `?day=YYYY-MM-DD` selects a UTC day
/// (default: today); `?client=` narrows the aggregation to that client.
#[derive(Debug, Default, Deserialize)]
pub(crate) struct CostParams {
    pub(crate) day: Option<String>,
    pub(crate) client: Option<String>,
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
    // Present only for `?client=<id>` requests (v0.4 M3): the client the view
    // was resolved for, and whether it fell back to the global target (no
    // scoped entry). Absent otherwise, so the default shape is unchanged.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) client: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) follows_global: Option<bool>,
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
pub(crate) struct SwitchView {
    requested: String,
    target: String,
    primary: String,
    fallback: Vec<String>,
    policy: PolicyView,
    // Present only for scoped switches (`?client=<id>`); absent for global.
    #[serde(skip_serializing_if = "Option::is_none")]
    client: Option<String>,
}

/// One `/_ccm/clients` entry: a client's runtime switch state.
#[derive(Serialize)]
pub(crate) struct ClientView {
    pub(crate) client: String,
    pub(crate) target: String,
    pub(crate) requests: u64,
    pub(crate) last_seen_ms: u64,
}

#[derive(Serialize)]
struct CircuitView {
    model: String,
    state: String,
    consecutive_failures: usize,
    open_until_ms: Option<u64>,
}

pub(crate) fn control_router(state: ProxyState) -> Router {
    let router = Router::new()
        .route("/health", get(health))
        .route("/_ccm/status", get(control_status))
        .route("/_ccm/models", get(control_models))
        .route("/_ccm/routes", get(control_routes))
        .route("/_ccm/traces", get(control_traces))
        .route("/_ccm/circuits", get(control_circuits))
        .route("/_ccm/metrics", get(control_metrics))
        .route("/_ccm/scores", get(control_scores))
        .route("/_ccm/decisions", get(control_decisions))
        .route("/_ccm/usage", get(control_usage))
        .route("/_ccm/cost", get(control_cost))
        .route("/_ccm/clients", get(control_clients))
        .route("/_ccm/switch/{target}", post(control_switch))
        .route("/v1/messages", any(forward_messages));
    // The exporter route exists only when the exporter is on (v0.4 M7):
    // `prometheus_enabled = false` removes `/metrics` from the listener
    // entirely. It shares the ONE guarded listener — no second port
    // (invariant 11).
    let router = match state.prom {
        Some(_) => router.route("/metrics", get(control_metrics_exposition)),
        None => router,
    };
    router.with_state(state)
}

async fn health() -> &'static str {
    "ok"
}

pub(crate) async fn control_status(
    State(state): State<ProxyState>,
    Query(params): Query<ClientParams>,
) -> impl IntoResponse {
    // `?client=<id>` shows that client's EFFECTIVE view: its entry target if
    // switched, else the global target (indicated by `follows_global`).
    let global = state.target.read().await.clone();
    let (target, follows_global) = match params.client.as_deref() {
        Some(id) => match state.clients.read().await.get(id) {
            Some(entry) => (entry.target.clone(), Some(false)),
            None => (global, Some(true)),
        },
        None => (global, None),
    };
    match AppConfig::load().and_then(|config| status_view(&config, &target)) {
        Ok(mut view) => {
            view.client = params.client.clone();
            view.follows_global = follows_global;
            Json(view).into_response()
        }
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

pub(crate) async fn control_traces(
    State(state): State<ProxyState>,
    Query(params): Query<ClientParams>,
) -> Json<Vec<AttemptTrace>> {
    let traces = state
        .traces
        .read()
        .await
        .iter()
        .filter(|trace| client_matches(params.client.as_deref(), &trace.client))
        .cloned()
        .collect();
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

/// `GET /metrics` — the Prometheus text exposition (v0.4 M7), hand-rendered
/// from a snapshot of the exporter counters plus the runtime views the
/// `/_ccm` endpoints serve (attempts derive from the same per-model metrics
/// map, so the two surfaces agree on every SETTLED attempt; an attempt
/// still in flight exists only in `/_ccm/metrics`, as it always has).
/// Content type per the text format 0.0.4 convention.
async fn control_metrics_exposition(State(state): State<ProxyState>) -> Response<Body> {
    let Some(prom) = state.prom.as_deref() else {
        // Unreachable through the router (the route is registered only when
        // the exporter exists); kept honest for direct calls.
        return control_error(
            StatusCode::NOT_FOUND,
            anyhow::anyhow!("metrics export disabled"),
        );
    };
    let now = now_ms();
    let mut models: Vec<(String, ModelMetrics)> = state
        .metrics
        .read()
        .await
        .iter()
        .map(|(model, metrics)| (model.clone(), metrics.clone()))
        .collect();
    models.sort_by(|left, right| left.0.cmp(&right.0));
    let mut circuits: Vec<(String, crate::prometheus::CircuitInput)> = state
        .circuits
        .read()
        .await
        .iter()
        .map(|(model, circuit)| {
            (
                model.clone(),
                crate::prometheus::CircuitInput {
                    // "Open" in the skipping sense, matching the OPEN and
                    // HALF_OPEN rows of `/_ccm/circuits`: cooldown unexpired,
                    // or a half-open probe holding the single slot.
                    open: circuit.open_until_ms.is_some_and(|until| until > now)
                        || circuit.half_open_probe_in_flight,
                    consecutive_failures: circuit.consecutive_failures,
                },
            )
        })
        .collect();
    circuits.sort_by(|left, right| left.0.cmp(&right.0));

    let text = crate::prometheus::render(&crate::prometheus::Snapshot {
        process_start_seconds: crate::prometheus::process_start_seconds(),
        history_dropped: state.history.dropped_count(),
        prom: prom.snapshot(),
        models,
        circuits,
    });
    (
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        text,
    )
        .into_response()
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

pub(crate) async fn control_decisions(
    State(state): State<ProxyState>,
    Query(params): Query<DecisionParams>,
) -> impl IntoResponse {
    // No history filters: the in-memory ring, byte-identical to the v0.3
    // behavior (last 100, newest last, `?client=` filter) — except that an
    // explicit `?limit=` truncates the view to the most recent N, so a
    // paginating caller gets the same parameter semantics on both branches
    // instead of a silently ignored value.
    if params.since.is_none() && params.until.is_none() && params.model.is_none() {
        let mut decisions: Vec<RoutingDecision> = state
            .decisions
            .read()
            .await
            .iter()
            .filter(|decision| client_matches(params.client.as_deref(), &decision.client))
            .cloned()
            .collect();
        if let Some(limit) = params.limit {
            if decisions.len() > limit {
                decisions.drain(..decisions.len() - limit);
            }
        }
        return Json(decisions).into_response();
    }
    // Any of since/until/model: a disk read over the persisted history
    // (oldest first), still honoring the client filter. The read is bounded,
    // not just the response: `read_decisions` walks files newest-first and
    // stops once `limit` matches are held, so a routine dashboard query
    // costs O(limit + one file) — deserializing the entire retained history
    // (hundreds of MB under a busy proxy) would spike memory, and absent an
    // explicit `?limit=` the most recent 1000 records come back. The read is
    // blocking file IO — run it off the workers that serve /v1/messages.
    const DEFAULT_DISK_LIMIT: usize = 1000;
    let Some(dir) = state.history.dir() else {
        return control_error(
            StatusCode::BAD_REQUEST,
            anyhow::anyhow!(
                "history query parameters (since/until/model) require a running history store; check [observability] history_enabled and that no other proxy holds the single-writer lock"
            ),
        );
    };
    let query = crate::history::DecisionQuery {
        since: params.since,
        until: params.until,
        model: params.model.clone(),
        client: params.client.clone(),
        limit: Some(params.limit.unwrap_or(DEFAULT_DISK_LIMIT)),
    };
    let dir = dir.to_path_buf();
    match tokio::task::spawn_blocking(move || crate::history::read_decisions(&dir, &query)).await {
        Ok(Ok(decisions)) => Json(decisions).into_response(),
        Ok(Err(err)) => control_error(StatusCode::INTERNAL_SERVER_ERROR, err),
        Err(err) => control_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            anyhow::anyhow!("history read task failed: {err}"),
        ),
    }
}

/// `/_ccm/usage` (v0.4 M6): the usage-record mirror of
/// `/_ccm/decisions` — the in-memory ring without history filters, a
/// bounded disk read with any of them.
pub(crate) async fn control_usage(
    State(state): State<ProxyState>,
    Query(params): Query<UsageParams>,
) -> impl IntoResponse {
    if params.since.is_none() && params.until.is_none() && params.model.is_none() {
        let mut records: Vec<crate::usage::UsageRecord> = state
            .usage
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .filter(|record| client_matches(params.client.as_deref(), &record.client))
            .cloned()
            .collect();
        if let Some(limit) = params.limit {
            if records.len() > limit {
                records.drain(..records.len() - limit);
            }
        }
        return Json(records).into_response();
    }
    const DEFAULT_DISK_LIMIT: usize = 1000;
    let Some(dir) = state.history.dir() else {
        return control_error(
            StatusCode::BAD_REQUEST,
            anyhow::anyhow!(
                "history query parameters (since/until/model) require a running history store; check [observability] history_enabled and that no other proxy holds the single-writer lock"
            ),
        );
    };
    let query = crate::history::UsageQuery {
        since: params.since,
        until: params.until,
        model: params.model.clone(),
        client: params.client.clone(),
        limit: Some(params.limit.unwrap_or(DEFAULT_DISK_LIMIT)),
    };
    let dir = dir.to_path_buf();
    match tokio::task::spawn_blocking(move || crate::history::read_usage(&dir, &query)).await {
        Ok(Ok(records)) => Json(records).into_response(),
        Ok(Err(err)) => control_error(StatusCode::INTERNAL_SERVER_ERROR, err),
        Err(err) => control_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            anyhow::anyhow!("history read task failed: {err}"),
        ),
    }
}

/// `/_ccm/cost?day=` (v0.4 M6): usage and cost aggregated by model over one
/// UTC calendar day (default: today). Always a disk read — a day of records
/// does not fit the in-memory ring, and cost analysis is a history feature.
/// The day window is inclusive of the whole day: `[midnight, next
/// midnight)`.
pub(crate) async fn control_cost(
    State(state): State<ProxyState>,
    Query(params): Query<CostParams>,
) -> impl IntoResponse {
    let day = params
        .day
        .clone()
        .unwrap_or_else(|| crate::date::utc_day_of(now_ms()));
    let Some(since) = crate::date::parse_utc_day(&day) else {
        return control_error(
            StatusCode::BAD_REQUEST,
            anyhow::anyhow!("invalid ?day=`{day}`: expected YYYY-MM-DD (UTC)"),
        );
    };
    let Some(dir) = state.history.dir() else {
        return control_error(
            StatusCode::BAD_REQUEST,
            anyhow::anyhow!(
                "cost aggregation requires a running history store; check [observability] history_enabled and that no other proxy holds the single-writer lock"
            ),
        );
    };
    let query = crate::history::UsageQuery {
        since: Some(since),
        // next midnight minus 1ms: the inclusive end of the day
        until: Some(since + 86_400_000 - 1),
        model: None,
        client: params.client.clone(),
        // No limit: an aggregation must see the whole day, not the newest
        // slice of it. Retention bounds the scan in practice.
        limit: None,
    };
    let dir = dir.to_path_buf();
    match tokio::task::spawn_blocking(move || {
        crate::history::read_usage(&dir, &query).map(|records| {
            let aggregation = crate::usage::aggregate_costs(&records);
            (records, aggregation)
        })
    })
    .await
    {
        Ok(Ok((records, aggregation))) => Json(CostView {
            day,
            since,
            until: since + 86_400_000 - 1,
            client: params.client,
            requests: aggregation.total_requests,
            models: aggregation.models,
            total_cost_usd: aggregation.total_cost_usd,
            total_unpriced_requests: aggregation.total_unpriced_requests,
            complete: records.iter().filter(|record| record.complete).count(),
        })
        .into_response(),
        Ok(Err(err)) => control_error(StatusCode::INTERNAL_SERVER_ERROR, err),
        Err(err) => control_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            anyhow::anyhow!("history read task failed: {err}"),
        ),
    }
}

/// Response shape of `/_ccm/cost`: one day's aggregation.
#[derive(Serialize)]
struct CostView {
    day: String,
    since: u64,
    until: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    client: Option<String>,
    requests: u64,
    models: Vec<crate::usage::CostByModel>,
    total_cost_usd: f64,
    total_unpriced_requests: u64,
    complete: usize,
}

/// One client's runtime entry, as listed by `/_ccm/clients`.
pub(crate) async fn control_clients(State(state): State<ProxyState>) -> Json<Vec<ClientView>> {
    let clients = state.clients.read().await;
    let mut views = clients
        .iter()
        .map(|(client, entry)| ClientView {
            client: client.clone(),
            target: entry.target.clone(),
            requests: entry.requests,
            last_seen_ms: entry.last_seen_ms,
        })
        .collect::<Vec<_>>();
    views.sort_by(|left, right| left.client.cmp(&right.client));
    Json(views)
}

/// A `?client=` filter matches records tagged with exactly that client; no
/// filter passes everything through unchanged.
fn client_matches(filter: Option<&str>, client: &Option<String>) -> bool {
    match filter {
        Some(id) => client.as_deref() == Some(id),
        None => true,
    }
}

pub(crate) async fn control_switch(
    State(state): State<ProxyState>,
    Path(target): Path<String>,
    Query(params): Query<ClientParams>,
) -> impl IntoResponse {
    match apply_switch(&state, &target, params.client.as_deref()).await {
        Ok(view) => Json(view).into_response(),
        Err(err) => control_error(StatusCode::BAD_REQUEST, err),
    }
}

/// Switch logic shared by the HTTP handler and the integration test. Without
/// a client id this is the v0.3 global switch (runtime-only, invariant 8);
/// with one it create-or-updates that client's in-memory entry — never
/// `state.toml` (invariant 9) and never the global target. Invalid id charset
/// and unknown targets are errors (both surface as HTTP 400).
pub(crate) async fn apply_switch(
    state: &ProxyState,
    requested: &str,
    client: Option<&str>,
) -> Result<SwitchView> {
    if let Some(id) = client {
        if !valid_client_id(id) {
            bail!("{}", invalid_client_id_message(id));
        }
    }
    let config = AppConfig::load()?;
    let route = config.resolve_route(requested)?;
    match client {
        Some(id) => {
            let mut clients = state.clients.write().await;
            let entry = clients
                .entry(id.to_string())
                .or_insert_with(ClientEntry::default);
            entry.target = route.target.clone();
            entry.last_seen_ms = now_ms();
            // `requests` is a usage counter and survives a re-switch.
        }
        None => {
            *state.target.write().await = route.target.clone();
        }
    }
    Ok(SwitchView {
        requested: requested.to_string(),
        target: route.target,
        primary: route.primary,
        fallback: route.fallback,
        policy: PolicyView::from(&route.policy),
        client: client.map(str::to_string),
    })
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
        client: None,
        follows_global: None,
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

/// Unit tests for the pure view builders (moved from proxy.rs's test module
/// in the v0.4 M0 split cleanup — they exercise this module, not the
/// forwarding path). The handler behavior lives in proxy.rs's integration
/// tests, which drive these views through the real Router.
#[cfg(test)]
mod tests {
    use super::*;

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
}
