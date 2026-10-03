//! Candidate ranking for the five selection strategies, plus the score
//! helpers shared by decision building and the `/_ccm/scores` view.

use std::cmp::Ordering;

use serde::Serialize;

use crate::{
    config::AppConfig,
    proxy::ProxyState,
    route::{SelectionStrategy, SelectionWeights},
    routing::metrics::{success_rate, ModelMetrics},
};

const HEALTH_MIN_SAMPLES: u64 = 3;

#[derive(Clone, Serialize)]
pub(crate) struct CandidateScoreView {
    pub(crate) model: String,
    pub(crate) reliability_score: f64,
    pub(crate) latency_score: f64,
    pub(crate) cost_score: f64,
    pub(crate) quality_score: f64,
    pub(crate) weighted_score: f64,
    pub(crate) attempts: u64,
    pub(crate) latency_ewma_ms: Option<f64>,
    pub(crate) cost_weight: f64,
    pub(crate) quality_weight: f64,
}

pub(crate) async fn select_candidates(
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
            candidates
                .sort_by(|left, right| compare_latency(metrics.get(left), metrics.get(right)));
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

pub(crate) fn candidate_health_rank(metrics: Option<&ModelMetrics>) -> f64 {
    match metrics {
        Some(metrics) if metrics.attempts >= HEALTH_MIN_SAMPLES => success_rate(metrics),
        _ => 1.0,
    }
}

pub(crate) fn weighted_score(
    config: &AppConfig,
    metrics: Option<&ModelMetrics>,
    model: &str,
    weights: &SelectionWeights,
) -> f64 {
    let reliability = candidate_health_rank(metrics);
    let latency = latency_score(metrics);
    let cost = cost_score(config, model);
    let quality = quality_score(config, model);
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

pub(crate) fn candidate_score_view(
    config: &AppConfig,
    metrics: Option<&ModelMetrics>,
    model: &str,
    weights: &SelectionWeights,
) -> CandidateScoreView {
    CandidateScoreView {
        model: model.to_string(),
        reliability_score: candidate_health_rank(metrics),
        latency_score: latency_score(metrics),
        cost_score: cost_score(config, model),
        quality_score: quality_score(config, model),
        weighted_score: weighted_score(config, metrics, model, weights),
        attempts: metrics.map(|metrics| metrics.attempts).unwrap_or(0),
        latency_ewma_ms: metrics.and_then(|metrics| metrics.latency_ewma_ms),
        cost_weight: model_cost(config, model),
        quality_weight: model_quality(config, model),
    }
}

pub(crate) fn cost_score(config: &AppConfig, model: &str) -> f64 {
    1.0 / (1.0 + model_cost(config, model).max(0.0))
}

pub(crate) fn quality_score(config: &AppConfig, model: &str) -> f64 {
    model_quality(config, model).clamp(0.0, 1.0)
}

pub(crate) fn latency_score(metrics: Option<&ModelMetrics>) -> f64 {
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

pub(crate) fn selection_name(selection: &SelectionStrategy) -> &'static str {
    match selection {
        SelectionStrategy::Ordered => "ordered",
        SelectionStrategy::Healthiest => "healthiest",
        SelectionStrategy::LowestLatency => "lowest-latency",
        SelectionStrategy::LowestCost => "lowest-cost",
        SelectionStrategy::Weighted => "weighted",
    }
}
