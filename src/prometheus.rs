//! Prometheus text-format exporter (v0.4 M7): hand-rendered exposition on
//! the proxy's ONE listener (`GET /metrics`), no exporter crate, no new
//! dependencies. The loopback-only bind guard already covers this route —
//! there is deliberately no second port (V0.4_PLAN §3.3, invariant 11).
//!
//! Split:
//! - [`PromState`] — the mutable half: plain counters and fixed-bucket
//!   histograms behind a std `Mutex` (poison-recovering like the usage ring,
//!   because one hook site — the usage finalize closure — runs inside
//!   `poll_next`/`Drop`, where awaiting is impossible). Every method is a
//!   brief lock-and-bump; recording metrics must never slow a request.
//! - [`render`] — the pure half: a [`Snapshot`] (plain owned data) into the
//!   exact exposition text. Tests pin the output byte-for-byte.
//!
//! Dual-track latency (§3.3): the fixed-bucket `ccm_header_latency_seconds`
//! histogram lets a Prometheus server compute `histogram_quantile` over any
//! scrape window (including across proxy restarts, since the buckets are
//! cumulative at each scrape), while the in-memory EWMA stays available as
//! the `ccm_latency_ewma_ms` gauge for a quick glance.
//!
//! Honest boundaries (mirroring the M6 cost view): token/cost counters cover
//! only ACCEPTED 2xx responses (the usage-capture contract), prices are
//! hand-entered, and cost is exported as integer micro-USD — proxy-side
//! measurement, not bill truth. Runtime counters reset on restart by design
//! (same reasoning as every other in-memory observable); history queries
//! read disk.

use std::collections::BTreeMap;
use std::sync::{Mutex, OnceLock};

use crate::routing::metrics::ModelMetrics;

/// Process start, pinned by `serve()` just before the listener is built —
/// the closest honest approximation of "when this proxy started serving".
/// Falls back to first read when never pinned (test states constructed
/// directly), so the gauge always has a sane value.
static PROCESS_START: OnceLock<u64> = OnceLock::new();

/// Pin the process-start gauge. Called once, from `proxy::serve`.
pub(crate) fn note_process_start() {
    let _ = PROCESS_START.set(crate::date::now_ms() / 1000);
}

/// The pinned process start in whole seconds (sub-second precision is noise
/// for a gauge meant to join against `up`).
pub(crate) fn process_start_seconds() -> u64 {
    *PROCESS_START.get_or_init(|| crate::date::now_ms() / 1000)
}

/// Fixed histogram upper bounds, milliseconds, shared by both histograms.
/// Coarse on purpose: enough resolution for header-latency and
/// decision-duration questions at a glance, tiny cardinality in memory.
const BUCKETS_MS: [f64; 11] = [
    5.0, 10.0, 25.0, 50.0, 100.0, 250.0, 500.0, 1000.0, 2500.0, 5000.0, 10000.0,
];

// ===========================================================================
// Histogram
// ===========================================================================

/// Cumulative fixed-bucket histogram: `buckets[i]` counts observations
/// `<= BUCKETS_MS[i]`; `count` is the `+Inf` bucket. Bounds are compared
/// inclusively (`le`).
#[derive(Clone, Debug, Default)]
pub(crate) struct Histogram {
    buckets: Vec<u64>,
    count: u64,
    sum_ms: f64,
}

impl Histogram {
    /// Observe one duration in milliseconds. Non-finite or negative values
    /// (which would poison sums and sort outside every bucket) are dropped.
    pub(crate) fn observe_ms(&mut self, ms: f64) {
        if ms.is_nan() || ms.is_infinite() || ms < 0.0 {
            return; // NaN, ±Inf, or negative
        }
        if self.buckets.is_empty() {
            self.buckets = vec![0; BUCKETS_MS.len()];
        }
        self.sum_ms += ms;
        self.count += 1;
        for (index, bound) in BUCKETS_MS.iter().enumerate() {
            if ms <= *bound {
                self.buckets[index] += 1;
            }
        }
    }

    fn bucket(&self, index: usize) -> u64 {
        self.buckets.get(index).copied().unwrap_or(0)
    }
}

// ===========================================================================
// Mutable state
// ===========================================================================

#[derive(Default)]
struct PromInner {
    /// `(target, outcome) -> count`. Outcome is the normalized decision
    /// verdict: `success` | `error`.
    requests: BTreeMap<(String, String), u64>,
    /// Time to upstream response headers, per model alias.
    header_latency: BTreeMap<String, Histogram>,
    /// Routing-decision duration (request arrival → accepted headers or
    /// terminal failure), per target.
    decision_duration: BTreeMap<String, Histogram>,
    /// `(model, kind) -> tokens`, from usage records (accepted 2xx only).
    tokens: BTreeMap<(String, &'static str), u64>,
    /// Accumulated USD cost per model (f64, §8.2); exported as integer
    /// micro-USD. Unpriced records add nothing.
    cost_usd: BTreeMap<String, f64>,
}

/// The exporter's mutable half. Every method locks briefly and recovers from
/// a poisoned lock (same policy as the usage ring): observability must not
/// take the proxy down with it.
pub(crate) struct PromState {
    inner: Mutex<PromInner>,
}

impl Default for PromState {
    fn default() -> Self {
        Self {
            inner: Mutex::new(PromInner::default()),
        }
    }
}

/// Normalize a `RoutingDecision.outcome` into a label value. The decision
/// outcome strings are `HTTP <code> <reason…>` (StatusCode's Display keeps
/// the canonical reason, e.g. `HTTP 200 OK`) for accepted responses and
/// terminal passthroughs, or free-form failures (`failed: ...`, `resolve
/// error: ...`, `no candidate admitted ...`); only a 2xx accepted response
/// is `success`. Note this is the decision-level verdict: a 200 stream that
/// later fails mid-flight still counts `success` here — the honest
/// `complete: false` signal lives in the usage records (M6 semantics).
pub(crate) fn decision_outcome(outcome: &str) -> &'static str {
    let Some(status) = outcome.strip_prefix("HTTP ") else {
        return "error";
    };
    match status
        .split_whitespace()
        .next()
        .and_then(|code| code.parse::<u16>().ok())
    {
        Some(code) if (200..300).contains(&code) => "success",
        _ => "error",
    }
}

impl PromState {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// One proxied request reached a terminal verdict.
    pub(crate) fn record_request(&self, target: &str, outcome: &str) {
        let mut inner = self.lock();
        *inner
            .requests
            .entry((target.to_string(), outcome.to_string()))
            .or_insert(0) += 1;
    }

    /// Header latency of one upstream attempt (success or failure — the
    /// same samples the EWMA sees).
    pub(crate) fn observe_header_latency(&self, model: &str, latency_ms: f64) {
        self.lock()
            .header_latency
            .entry(model.to_string())
            .or_default()
            .observe_ms(latency_ms);
    }

    /// Full routing-decision duration of one proxied request.
    pub(crate) fn observe_decision_duration(&self, target: &str, duration_ms: f64) {
        self.lock()
            .decision_duration
            .entry(target.to_string())
            .or_default()
            .observe_ms(duration_ms);
    }

    /// One usage record finalized (accepted 2xx response): token counters
    /// and (when priced) accumulated cost.
    pub(crate) fn record_usage(
        &self,
        model: &str,
        input: u64,
        output: u64,
        cache_read: u64,
        cache_write: u64,
        cost_usd: Option<f64>,
    ) {
        let mut inner = self.lock();
        let mut bump = |kind: &'static str, tokens: u64| {
            if tokens > 0 {
                *inner.tokens.entry((model.to_string(), kind)).or_insert(0) += tokens;
            }
        };
        bump("input", input);
        bump("output", output);
        bump("cache_read", cache_read);
        bump("cache_write", cache_write);
        if let Some(cost) = cost_usd {
            *inner.cost_usd.entry(model.to_string()).or_insert(0.0) += cost;
        }
    }

    /// Owned copy of the exporter-owned counters, for [`render`].
    pub(crate) fn snapshot(&self) -> PromSnapshot {
        let inner = self.lock();
        PromSnapshot {
            requests: inner.requests.clone(),
            header_latency: inner.header_latency.clone(),
            decision_duration: inner.decision_duration.clone(),
            tokens: inner.tokens.clone(),
            cost_usd: inner.cost_usd.clone(),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, PromInner> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// Owned exporter half of a render input. BTreeMaps keep the output
/// deterministic by construction.
pub(crate) struct PromSnapshot {
    pub(crate) requests: BTreeMap<(String, String), u64>,
    pub(crate) header_latency: BTreeMap<String, Histogram>,
    pub(crate) decision_duration: BTreeMap<String, Histogram>,
    pub(crate) tokens: BTreeMap<(String, &'static str), u64>,
    pub(crate) cost_usd: BTreeMap<String, f64>,
}

// ===========================================================================
// Pure render
// ===========================================================================

/// One circuit's render input.
pub(crate) struct CircuitInput {
    pub(crate) open: bool,
    pub(crate) consecutive_failures: usize,
}

/// Everything one exposition needs. The handler assembles it (proxy state +
/// history drop counter + process start); `render` stays pure and testable.
pub(crate) struct Snapshot {
    pub(crate) process_start_seconds: u64,
    pub(crate) history_dropped: u64,
    pub(crate) prom: PromSnapshot,
    /// Per-model runtime metrics, sorted by model (the caller sorts).
    pub(crate) models: Vec<(String, ModelMetrics)>,
    /// Circuit gauges, sorted by model (the caller sorts).
    pub(crate) circuits: Vec<(String, CircuitInput)>,
}

/// Escape a label value per the text format: `\` → `\\`, `"` → `\"`,
/// newline → `\n`.
fn escape_label(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            other => out.push(other),
        }
    }
    out
}

/// Milliseconds → the seconds value the exposition prints (e.g. `5.0` →
/// `0.005`, `10000.0` → `10`). f64 `Display` already produces the shortest
/// roundtrip form Prometheus parses.
fn secs(ms: f64) -> String {
    format!("{}", ms / 1000.0)
}

/// Render the exposition text (Prometheus text format 0.0.4). Family
/// HELP/TYPE headers are always emitted — an idle proxy still advertises
/// its families — while series lines appear only where data exists.
pub(crate) fn render(snapshot: &Snapshot) -> String {
    let mut out = String::with_capacity(2048);

    // --- liveness / process ---
    push_help(
        &mut out,
        "ccm_up",
        "gauge",
        "Whether this CCM proxy is serving.",
    );
    out.push_str("ccm_up 1\n");
    push_help(
        &mut out,
        "ccm_process_start_time_seconds",
        "gauge",
        "Unix time the proxy process started serving.",
    );
    out.push_str(&format!(
        "ccm_process_start_time_seconds {}\n",
        snapshot.process_start_seconds
    ));

    // --- proxied requests by target / outcome ---
    push_help(
        &mut out,
        "ccm_proxy_requests_total",
        "counter",
        "Proxied requests by target and decision outcome (success = accepted 2xx response).",
    );
    for ((target, outcome), count) in &snapshot.prom.requests {
        out.push_str(&format!(
            "ccm_proxy_requests_total{{target=\"{}\",outcome=\"{}\"}} {count}\n",
            escape_label(target),
            escape_label(outcome),
        ));
    }

    // --- upstream attempts by model / outcome (derived from the same
    //     counters /_ccm/metrics serves). The five outcomes are disjoint
    //     and, once every counted attempt has settled, sum to `attempts`;
    //     an attempt still in flight (or aborted mid-header-wait) exists
    //     only in `attempts` until it settles — the same in-flight gap
    //     `/_ccm/metrics` has always had. ---
    push_help(
        &mut out,
        "ccm_attempts_total",
        "counter",
        "Upstream attempts by model and outcome (success, http_error, rate_limited, timeout, request_error).",
    );
    for (model, metrics) in &snapshot.models {
        // 429 increments both `http_errors` and `rate_limited`; subtract it
        // back out so the five series stay disjoint.
        let http_error = metrics.http_errors.saturating_sub(metrics.rate_limited);
        for (outcome, count) in [
            ("success", metrics.successes),
            ("http_error", http_error),
            ("rate_limited", metrics.rate_limited),
            ("timeout", metrics.timeouts),
            ("request_error", metrics.request_errors),
        ] {
            out.push_str(&format!(
                "ccm_attempts_total{{model=\"{}\",outcome=\"{}\"}} {count}\n",
                escape_label(model),
                outcome,
            ));
        }
    }

    // --- histograms ---
    push_help(
        &mut out,
        "ccm_header_latency_seconds",
        "histogram",
        "Time to upstream response headers per model.",
    );
    for (model, histogram) in &snapshot.prom.header_latency {
        push_histogram(
            &mut out,
            "ccm_header_latency_seconds",
            "model",
            model,
            histogram,
        );
    }
    push_help(
        &mut out,
        "ccm_decision_duration_seconds",
        "histogram",
        "Full routing-decision duration per target (request arrival to accepted headers or terminal failure).",
    );
    for (target, histogram) in &snapshot.prom.decision_duration {
        push_histogram(
            &mut out,
            "ccm_decision_duration_seconds",
            "target",
            target,
            histogram,
        );
    }

    // --- EWMA gauge (the second latency track) ---
    push_help(
        &mut out,
        "ccm_latency_ewma_ms",
        "gauge",
        "EWMA (alpha 0.2) of upstream header latency in milliseconds; absent before the first sample.",
    );
    for (model, metrics) in &snapshot.models {
        if let Some(ewma) = metrics.latency_ewma_ms {
            out.push_str(&format!(
                "ccm_latency_ewma_ms{{model=\"{}\"}} {ewma}\n",
                escape_label(model)
            ));
        }
    }

    // --- circuits ---
    push_help(
        &mut out,
        "ccm_circuit_open",
        "gauge",
        "1 while the model's circuit breaker is skipping it (OPEN cooldown or a HALF_OPEN probe in flight).",
    );
    for (model, circuit) in &snapshot.circuits {
        out.push_str(&format!(
            "ccm_circuit_open{{model=\"{}\"}} {}\n",
            escape_label(model),
            u8::from(circuit.open),
        ));
    }
    push_help(
        &mut out,
        "ccm_circuit_consecutive_failures",
        "gauge",
        "Consecutive upstream failures currently counted by the model's circuit breaker.",
    );
    for (model, circuit) in &snapshot.circuits {
        out.push_str(&format!(
            "ccm_circuit_consecutive_failures{{model=\"{}\"}} {}\n",
            escape_label(model),
            circuit.consecutive_failures,
        ));
    }

    // --- usage (accepted 2xx responses only; proxy-side measurement) ---
    push_help(
        &mut out,
        "ccm_tokens_total",
        "counter",
        "Tokens reported by accepted responses, by model and kind (input, output, cache_read, cache_write).",
    );
    for ((model, kind), tokens) in &snapshot.prom.tokens {
        out.push_str(&format!(
            "ccm_tokens_total{{model=\"{}\",kind=\"{}\"}} {tokens}\n",
            escape_label(model),
            kind,
        ));
    }
    push_help(
        &mut out,
        "ccm_cost_micro_usd_total",
        "counter",
        "Accumulated cost of accepted responses in integer micro-USD, by model (hand-entered prices; unpriced models add nothing).",
    );
    for (model, cost) in &snapshot.prom.cost_usd {
        // A pathological hand-entered price can overflow f64 accumulation
        // to infinity; the honest exposition for "unrepresentable" is no
        // series, not a saturated u64::MAX that reads like real data
        // (validation already rejects negative/NaN prices at load).
        if !cost.is_finite() {
            continue;
        }
        out.push_str(&format!(
            "ccm_cost_micro_usd_total{{model=\"{}\"}} {}\n",
            escape_label(model),
            (cost * 1_000_000.0).round() as u64,
        ));
    }

    // --- history health ---
    push_help(
        &mut out,
        "ccm_history_dropped_total",
        "counter",
        "Observability records dropped (full writer queue or stopped writer) over this proxy's lifetime.",
    );
    out.push_str(&format!(
        "ccm_history_dropped_total {}\n",
        snapshot.history_dropped
    ));

    out
}

fn push_help(out: &mut String, name: &str, kind: &str, help: &str) {
    out.push_str(&format!("# HELP {name} {help}\n# TYPE {name} {kind}\n"));
}

/// One histogram series: bucket lines (finite bounds then `+Inf`), then sum
/// and count. `label_name` is the series' single label — `model` for header
/// latency, `target` for decision duration (per §3.3).
fn push_histogram(
    out: &mut String,
    name: &str,
    label_name: &str,
    label_value: &str,
    histogram: &Histogram,
) {
    for (index, bound) in BUCKETS_MS.iter().enumerate() {
        out.push_str(&format!(
            "{name}_bucket{{{label_name}=\"{}\",le=\"{}\"}} {}\n",
            escape_label(label_value),
            secs(*bound),
            histogram.bucket(index),
        ));
    }
    out.push_str(&format!(
        "{name}_bucket{{{label_name}=\"{}\",le=\"+Inf\"}} {}\n",
        escape_label(label_value),
        histogram.count,
    ));
    out.push_str(&format!(
        "{name}_sum{{{label_name}=\"{}\"}} {}\n",
        escape_label(label_value),
        secs(histogram.sum_ms),
    ));
    out.push_str(&format!(
        "{name}_count{{{label_name}=\"{}\"}} {}\n",
        escape_label(label_value),
        histogram.count,
    ));
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_snapshot() -> Snapshot {
        Snapshot {
            process_start_seconds: 1_790_000_000,
            history_dropped: 0,
            prom: PromSnapshot {
                requests: BTreeMap::new(),
                header_latency: BTreeMap::new(),
                decision_duration: BTreeMap::new(),
                tokens: BTreeMap::new(),
                cost_usd: BTreeMap::new(),
            },
            models: Vec::new(),
            circuits: Vec::new(),
        }
    }

    fn metrics(
        attempts: u64,
        successes: u64,
        http_errors: u64,
        rate_limited: u64,
        timeouts: u64,
        request_errors: u64,
    ) -> ModelMetrics {
        ModelMetrics {
            attempts,
            successes,
            http_errors,
            rate_limited,
            timeouts,
            request_errors,
            ..ModelMetrics::default()
        }
    }

    // 1. an idle proxy still advertises every family, in a fixed order, and
    //    nothing else — the exact empty exposition is pinned
    #[test]
    fn empty_render_is_pinned_byte_for_byte() {
        let text = render(&empty_snapshot());
        let expected = "\
# HELP ccm_up Whether this CCM proxy is serving.
# TYPE ccm_up gauge
ccm_up 1
# HELP ccm_process_start_time_seconds Unix time the proxy process started serving.
# TYPE ccm_process_start_time_seconds gauge
ccm_process_start_time_seconds 1790000000
# HELP ccm_proxy_requests_total Proxied requests by target and decision outcome (success = accepted 2xx response).
# TYPE ccm_proxy_requests_total counter
# HELP ccm_attempts_total Upstream attempts by model and outcome (success, http_error, rate_limited, timeout, request_error).
# TYPE ccm_attempts_total counter
# HELP ccm_header_latency_seconds Time to upstream response headers per model.
# TYPE ccm_header_latency_seconds histogram
# HELP ccm_decision_duration_seconds Full routing-decision duration per target (request arrival to accepted headers or terminal failure).
# TYPE ccm_decision_duration_seconds histogram
# HELP ccm_latency_ewma_ms EWMA (alpha 0.2) of upstream header latency in milliseconds; absent before the first sample.
# TYPE ccm_latency_ewma_ms gauge
# HELP ccm_circuit_open 1 while the model's circuit breaker is skipping it (OPEN cooldown or a HALF_OPEN probe in flight).
# TYPE ccm_circuit_open gauge
# HELP ccm_circuit_consecutive_failures Consecutive upstream failures currently counted by the model's circuit breaker.
# TYPE ccm_circuit_consecutive_failures gauge
# HELP ccm_tokens_total Tokens reported by accepted responses, by model and kind (input, output, cache_read, cache_write).
# TYPE ccm_tokens_total counter
# HELP ccm_cost_micro_usd_total Accumulated cost of accepted responses in integer micro-USD, by model (hand-entered prices; unpriced models add nothing).
# TYPE ccm_cost_micro_usd_total counter
# HELP ccm_history_dropped_total Observability records dropped (full writer queue or stopped writer) over this proxy's lifetime.
# TYPE ccm_history_dropped_total counter
ccm_history_dropped_total 0
";
        assert_eq!(text, expected);
    }

    // 2. label values are escaped per the text format
    #[test]
    fn label_values_are_escaped() {
        let mut snapshot = empty_snapshot();
        snapshot.prom.requests.insert(
            ("route\\x \"q\"\nline".to_string(), "success".to_string()),
            1,
        );
        let text = render(&snapshot);
        assert!(
            text.contains(
                "ccm_proxy_requests_total{target=\"route\\\\x \\\"q\\\"\\nline\",outcome=\"success\"} 1"
            ),
            "backslash, quote, and newline escaped: {text}"
        );
    }

    // 3. histogram buckets are cumulative and inclusive; sums and counts
    //    track every observation
    #[test]
    fn histogram_buckets_are_cumulative() {
        let mut histogram = Histogram::default();
        for ms in [4.0, 6.0, 600.0, 60_000.0] {
            histogram.observe_ms(ms);
        }
        histogram.observe_ms(f64::NAN); // dropped, not counted
        histogram.observe_ms(-1.0); // dropped too
        assert_eq!(histogram.count, 4);
        // bucket bounds: 5, 10, 25, 50, 100, 250, 500, 1000, 2500, 5000, 10000
        assert_eq!(histogram.bucket(0), 1, "le 5ms: the 4ms sample");
        assert_eq!(histogram.bucket(1), 2, "le 10ms: 4ms and 6ms");
        assert_eq!(histogram.bucket(7), 3, "le 1000ms: plus the 600ms sample");
        assert_eq!(histogram.bucket(10), 3, "le 10000ms: 60000ms overflows it");
        assert!((histogram.sum_ms - 60_610.0).abs() < 1e-9);

        // rendered bucket lines: finite bounds then +Inf, then sum/count
        let mut snapshot = empty_snapshot();
        snapshot
            .prom
            .header_latency
            .insert("glm".to_string(), histogram);
        let text = render(&snapshot);
        assert!(text.contains("ccm_header_latency_seconds_bucket{model=\"glm\",le=\"0.005\"} 1\n"));
        assert!(text.contains("ccm_header_latency_seconds_bucket{model=\"glm\",le=\"+Inf\"} 4\n"));
        assert!(text.contains("ccm_header_latency_seconds_count{model=\"glm\"} 4\n"));
        assert!(text.contains("ccm_header_latency_seconds_sum{model=\"glm\"} 60.61\n"));
    }

    // 4. attempts split into five DISJOINT outcomes that sum to `attempts`;
    //    a 429 counts once (rate_limited), not twice
    #[test]
    fn attempts_outcomes_are_disjoint_and_complete() {
        // 6 attempts = 2 success + 2 other-http-error + 1 four-twenty-nine
        // + 1 timeout (429 also bumped http_errors, as recorded)
        let model = metrics(6, 2, 3, 1, 1, 0);
        let mut snapshot = empty_snapshot();
        snapshot.models.push(("glm".to_string(), model));
        let text = render(&snapshot);
        for (outcome, count) in [
            ("success", "2"),
            ("http_error", "2"),
            ("rate_limited", "1"),
            ("timeout", "1"),
            ("request_error", "0"),
        ] {
            assert!(
                text.contains(&format!(
                    "ccm_attempts_total{{model=\"glm\",outcome=\"{outcome}\"}} {count}\n"
                )),
                "missing {outcome} series"
            );
        }
    }

    // 5. decision outcome normalization: only an accepted 2xx is success
    #[test]
    fn decision_outcome_normalization() {
        // The shapes forward() actually writes: StatusCode's Display keeps
        // the canonical reason phrase.
        assert_eq!(decision_outcome("HTTP 200 OK"), "success");
        assert_eq!(decision_outcome("HTTP 429 Too Many Requests"), "error");
        assert_eq!(decision_outcome("HTTP 503 Service Unavailable"), "error");
        // Hand-built / degenerate forms.
        assert_eq!(decision_outcome("HTTP 200"), "success");
        assert_eq!(decision_outcome("HTTP 299"), "success");
        assert_eq!(decision_outcome("HTTP 429"), "error");
        assert_eq!(decision_outcome("HTTP 502"), "error");
        assert_eq!(decision_outcome("HTTP "), "error");
        assert_eq!(
            decision_outcome("failed: glm: HTTP 429; kimi: timeout after 30s"),
            "error"
        );
        assert_eq!(
            decision_outcome("resolve error: provider `x` is not configured"),
            "error"
        );
        assert_eq!(
            decision_outcome("no candidate admitted or attempt budget exhausted"),
            "error"
        );
    }

    // 6. cost exports as integer micro-USD (rounding at export, f64 inside);
    //    the EWMA gauge stays absent until a first sample
    #[test]
    fn cost_exports_integer_micro_usd() {
        let mut snapshot = empty_snapshot();
        snapshot.prom.cost_usd.insert("glm".to_string(), 0.0000015); // 1.5 micro-USD
        snapshot.models.push((
            "glm".to_string(),
            ModelMetrics {
                latency_ewma_ms: None, // never sampled
                ..ModelMetrics::default()
            },
        ));
        snapshot.models.push((
            "kimi".to_string(),
            ModelMetrics {
                latency_ewma_ms: Some(204.5),
                ..ModelMetrics::default()
            },
        ));
        let text = render(&snapshot);
        assert!(
            text.contains("ccm_cost_micro_usd_total{model=\"glm\"} 2\n"),
            "1.5 rounds to 2"
        );
        assert!(
            !text.contains("ccm_latency_ewma_ms{model=\"glm\"}"),
            "unsampled EWMA stays absent"
        );
        assert!(text.contains("ccm_latency_ewma_ms{model=\"kimi\"} 204.5\n"));
    }

    // 7. PromState round trip: record through the public API, render the
    //    snapshot — token kinds, requests, and circuits all appear
    #[test]
    fn prom_state_records_and_renders() {
        let state = PromState::new();
        state.record_request("coding-route", "success");
        state.record_request("coding-route", "success");
        state.record_request("coding-route", "error");
        state.observe_header_latency("glm", 120.0);
        state.observe_decision_duration("coding-route", 2000.0);
        state.record_usage("glm", 100, 7, 40, 0, Some(0.000_412));
        state.record_usage("deepseek", 10, 0, 0, 0, None); // unpriced: no cost series
                                                           // pathological price overflow: the honest series is ABSENT, not a
                                                           // saturated u64::MAX (verify finding F4)
        state.record_usage("absurd", 1, 0, 0, 0, Some(f64::INFINITY));

        let mut snapshot = empty_snapshot();
        snapshot.prom = state.snapshot();
        snapshot.circuits.push((
            "glm".to_string(),
            CircuitInput {
                open: true,
                consecutive_failures: 3,
            },
        ));
        let text = render(&snapshot);

        assert!(text
            .contains("ccm_proxy_requests_total{target=\"coding-route\",outcome=\"success\"} 2\n"));
        assert!(text
            .contains("ccm_proxy_requests_total{target=\"coding-route\",outcome=\"error\"} 1\n"));
        assert!(text.contains("ccm_tokens_total{model=\"glm\",kind=\"input\"} 100\n"));
        assert!(text.contains("ccm_tokens_total{model=\"glm\",kind=\"output\"} 7\n"));
        assert!(text.contains("ccm_tokens_total{model=\"glm\",kind=\"cache_read\"} 40\n"));
        assert!(
            !text.contains("ccm_tokens_total{model=\"glm\",kind=\"cache_write\"}"),
            "zero-token kinds emit no series"
        );
        assert!(text.contains("ccm_cost_micro_usd_total{model=\"glm\"} 412\n"));
        assert!(
            !text.contains("ccm_cost_micro_usd_total{model=\"deepseek\"}"),
            "unpriced records add no cost"
        );
        assert!(
            !text.contains("ccm_cost_micro_usd_total{model=\"absurd\"}"),
            "non-finite accumulated cost emits no series, not u64::MAX"
        );
        assert!(text.contains("ccm_circuit_open{model=\"glm\"} 1\n"));
        assert!(text.contains("ccm_circuit_consecutive_failures{model=\"glm\"} 3\n"));
        // the two histograms carry DIFFERENT label names (§3.3): header
        // latency is model-scoped, decision duration is target-scoped
        assert!(text.contains("ccm_header_latency_seconds_count{model=\"glm\"} 1\n"));
        assert!(
            text.contains("ccm_decision_duration_seconds_count{target=\"coding-route\"} 1\n"),
            "decision duration labels by target, not model"
        );
        assert!(
            !text.contains("ccm_decision_duration_seconds_count{model="),
            "no model= spelling anywhere in the decision-duration family"
        );
    }
}
