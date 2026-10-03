//! Per-model runtime metrics: counters, the latency EWMA, and derived scores.

#[derive(Clone, Default)]
pub(crate) struct ModelMetrics {
    pub(crate) attempts: u64,
    pub(crate) successes: u64,
    pub(crate) http_errors: u64,
    pub(crate) fallback_failures: u64,
    pub(crate) timeouts: u64,
    pub(crate) request_errors: u64,
    pub(crate) rate_limited: u64,
    pub(crate) latency_ewma_ms: Option<f64>,
    pub(crate) last_success_ms: Option<u64>,
    pub(crate) last_failure_ms: Option<u64>,
}

const LATENCY_EWMA_ALPHA: f64 = 0.2;

pub(crate) fn update_latency_ewma(metrics: &mut ModelMetrics, latency_ms: f64) {
    metrics.latency_ewma_ms = Some(match metrics.latency_ewma_ms {
        Some(previous) => LATENCY_EWMA_ALPHA * latency_ms + (1.0 - LATENCY_EWMA_ALPHA) * previous,
        None => latency_ms,
    });
}

pub(crate) fn success_rate(metrics: &ModelMetrics) -> f64 {
    if metrics.attempts == 0 {
        0.0
    } else {
        metrics.successes as f64 / metrics.attempts as f64
    }
}

pub(crate) fn health_score(metrics: &ModelMetrics) -> Option<f64> {
    if metrics.attempts == 0 {
        None
    } else {
        Some(success_rate(metrics) * 100.0)
    }
}
