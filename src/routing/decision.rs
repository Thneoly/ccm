//! Routing decisions and attempt traces: the in-memory rings plus the
//! per-attempt recording family (metrics updates, trace log, decision store).

use std::{
    sync::atomic::Ordering as AtomicOrdering,
    time::{SystemTime, UNIX_EPOCH},
};

use serde::Serialize;

use crate::{
    config::AppConfig,
    proxy::ProxyState,
    route::RoutePolicy,
    routing::metrics::update_latency_ewma,
    routing::select::{
        candidate_health_rank, cost_score, latency_score, quality_score, selection_name,
        weighted_score,
    },
};

pub(crate) const TRACE_CAPACITY: usize = 100;
pub(crate) const DECISION_CAPACITY: usize = 100;

#[derive(Clone, Serialize)]
pub(crate) struct DecisionCandidate {
    pub(crate) rank: usize,
    pub(crate) model: String,
    pub(crate) reliability_score: f64,
    pub(crate) latency_score: f64,
    pub(crate) cost_score: f64,
    pub(crate) quality_score: f64,
    pub(crate) weighted_score: f64,
}

#[derive(Clone, Serialize)]
pub(crate) struct DecisionAttempt {
    pub(crate) attempt: usize,
    pub(crate) model: String,
    pub(crate) circuit: String,
    pub(crate) result: String,
    pub(crate) fallback: bool,
}

#[derive(Clone, Serialize)]
pub(crate) struct RoutingDecision {
    pub(crate) id: u64,
    pub(crate) timestamp_ms: u64,
    pub(crate) target: String,
    pub(crate) selection: String,
    pub(crate) configured_candidates: Vec<String>,
    pub(crate) ranked_candidates: Vec<DecisionCandidate>,
    pub(crate) attempts: Vec<DecisionAttempt>,
    pub(crate) selected: Option<String>,
    pub(crate) outcome: String,
}

#[derive(Clone, Serialize)]
pub(crate) struct AttemptTrace {
    timestamp_ms: u64,
    target: String,
    attempt: usize,
    model: String,
    result: String,
    fallback: bool,
}

pub(crate) async fn build_routing_decision(
    state: &ProxyState,
    config: &AppConfig,
    policy: &RoutePolicy,
    target: &str,
    configured_candidates: &[String],
    ranked_candidates: &[String],
) -> RoutingDecision {
    let metrics = state.metrics.read().await;
    let ranked_candidates = ranked_candidates
        .iter()
        .enumerate()
        .map(|(index, model)| DecisionCandidate {
            rank: index + 1,
            model: model.clone(),
            reliability_score: candidate_health_rank(metrics.get(model)),
            latency_score: latency_score(metrics.get(model)),
            cost_score: cost_score(config, model),
            quality_score: quality_score(config, model),
            weighted_score: weighted_score(config, metrics.get(model), model, &policy.weights),
        })
        .collect();

    RoutingDecision {
        id: state.decision_seq.fetch_add(1, AtomicOrdering::Relaxed),
        timestamp_ms: now_ms(),
        target: target.to_string(),
        selection: selection_name(&policy.selection).to_string(),
        configured_candidates: configured_candidates.to_vec(),
        ranked_candidates,
        attempts: Vec::new(),
        selected: None,
        outcome: "in-progress".to_string(),
    }
}

pub(crate) async fn store_decision(state: &ProxyState, decision: RoutingDecision) {
    let mut decisions = state.decisions.write().await;
    if decisions.len() == DECISION_CAPACITY {
        decisions.pop_front();
    }
    decisions.push_back(decision);
}

pub(crate) async fn record_attempt_started(state: &ProxyState, model: &str) {
    let mut metrics = state.metrics.write().await;
    metrics.entry(model.to_string()).or_default().attempts += 1;
}

pub(crate) async fn record_timeout(state: &ProxyState, model: &str, latency_ms: f64) {
    let mut metrics = state.metrics.write().await;
    let metric = metrics.entry(model.to_string()).or_default();
    metric.timeouts += 1;
    metric.last_failure_ms = Some(now_ms());
    update_latency_ewma(metric, latency_ms);
}

pub(crate) async fn record_request_error(state: &ProxyState, model: &str, latency_ms: f64) {
    let mut metrics = state.metrics.write().await;
    let metric = metrics.entry(model.to_string()).or_default();
    metric.request_errors += 1;
    metric.last_failure_ms = Some(now_ms());
    update_latency_ewma(metric, latency_ms);
}

pub(crate) async fn record_http_response(
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

pub(crate) async fn trace_attempt(
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

pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64
}
