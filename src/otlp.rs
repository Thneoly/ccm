//! OTLP/HTTP JSON metrics push (v0.5 M4): the dependency-discipline
//! extension of `prometheus.rs` — hand-rolled ExportMetricsServiceRequest
//! JSON, zero new dependencies (serde_json + reqwest are already direct;
//! the opentelemetry-crate alternative measured +31 net-new crates for
//! spec insurance a 12-family cumulative-only exporter does not need).
//!
//! Split, mirroring `prometheus.rs`:
//! - [`render_otel`] — the pure half: the SAME [`crate::prometheus::
//!   Snapshot`] the `/metrics` handler renders, into OTLP/JSON. No second
//!   source of truth: label sets, bucket bounds, and cost rounding are
//!   read from the shared snapshot types.
//! - [`push_once`] / the `serve()` push loop — the IO half: POST the
//!   rendered payload to `{endpoint}/v1/metrics` on a dedicated
//!   timeout-bounded client.
//!
//! Temporality: CUMULATIVE. Every interval re-sends the full state, so a
//! failed POST is NOT retried (nothing is lost — the next interval
//! re-sends) and failure logging is throttled.
//!
//! Credentials boundary (invariant 1): the payload carries only metric
//! names, label values, and counts. Collector auth headers, if ever
//! needed, come from the `OTEL_EXPORTER_OTLP_HEADERS` env var ONLY —
//! never config.toml, never the payload.
//!
//! Wire details (spec-derived; ONE live otelcol smoke is the acceptance
//! gate, V0.5_PLAN §5 M0/M4): protobuf JSON mapping renders 64-bit
//! integers (counts, timestamps) as decimal STRINGS; doubles render as
//! JSON numbers; `NumberDataPoint`'s `oneof value` members (`asInt` /
//! `asDouble`) sit DIRECTLY on the data-point object — protojson
//! flattens oneofs, there is no `value` wrapper key (only KeyValue
//! attributes have a real `value` field, for AnyValue); histograms carry
//! 11 `explicitBounds` in SECONDS and 12 per-bucket `bucketCounts`
//! DELTAS — differenced against PromState's cumulative buckets, whose
//! +Inf bucket is the histogram `count`.

use anyhow::{Context, Result};
use serde_json::{json, Value};

use crate::prometheus::{Histogram, Snapshot};

/// POST target under the configured base endpoint.
const METRICS_PATH: &str = "/v1/metrics";

/// How long one push may take end-to-end (in-repo precedent: the control
/// CLI client's 3s connect / 5s total, `control/mod.rs`). A stuck
/// collector must never accumulate stuck push tasks.
const PUSH_TIMEOUT_SECS: u64 = 5;

/// Aggregation temporality enum value: CUMULATIVE = 2.
const TEMPORALITY_CUMULATIVE: u64 = 2;

/// The collector base URL + `/v1/metrics`, appended exactly once: an
/// endpoint already naming the path is left alone.
pub(crate) fn metrics_url(endpoint: &str) -> String {
    let base = endpoint.trim_end_matches('/');
    if base.ends_with(METRICS_PATH) {
        base.to_string()
    } else {
        format!("{base}{METRICS_PATH}")
    }
}

/// Parse `OTEL_EXPORTER_OTLP_HEADERS` (the standard `k1=v1,k2=v2` form)
/// into request headers. Malformed pairs are skipped, not fatal — the
/// header set is best-effort plumbing, never a reason to stop exporting.
pub(crate) fn headers_from_env(raw: &str) -> Vec<(String, String)> {
    raw.split(',')
        .map(str::trim)
        .filter(|pair| !pair.is_empty())
        .filter_map(|pair| {
            let (key, value) = pair.split_once('=')?;
            let key = key.trim();
            if key.is_empty() {
                return None;
            }
            Some((key.to_string(), value.trim().to_string()))
        })
        .collect()
}

/// Stable resource `service.instance.id`: 16-hex FNV-1a of the CCM_HOME
/// path, distinguishing multiple CCM_HOMEs pushing to one collector
/// across restarts (a hash, so the path itself never leaves the box).
pub(crate) fn instance_id(home: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in home.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

/// One string attribute.
fn attr(key: &str, value: &str) -> Value {
    json!({"key": key, "value": {"stringValue": value}})
}

fn attrs(pairs: &[(&str, &str)]) -> Value {
    Value::Array(pairs.iter().map(|(k, v)| attr(k, v)).collect())
}

/// Milliseconds → seconds (f64), the unit the histogram bounds and sums
/// carry on the OTLP wire.
fn secs_f64(ms: f64) -> f64 {
    ms / 1000.0
}

/// Cumulative PromState buckets → the 12 per-bucket DELTAS OTLP wants
/// (11 finite bounds + the implicit +Inf tail).
fn bucket_deltas(histogram: &Histogram) -> Vec<String> {
    let mut deltas = Vec::with_capacity(crate::prometheus::BUCKETS_MS.len() + 1);
    let mut previous = 0u64;
    for index in 0..crate::prometheus::BUCKETS_MS.len() {
        let cumulative = histogram.bucket(index);
        deltas.push((cumulative - previous).to_string());
        previous = cumulative;
    }
    deltas.push((histogram.count() - previous).to_string());
    deltas
}

/// One monotone CUMULATIVE integer sum: int64 values as JSON strings per
/// the protobuf JSON mapping. Timestamps live on each data point.
fn int_sum(name: &str, description: &str, data_points: Vec<Value>) -> Value {
    json!({
        "name": name,
        "description": description,
        "sum": {
            "dataPoints": data_points,
            "aggregationTemporality": TEMPORALITY_CUMULATIVE,
            "isMonotonic": true,
        },
    })
}

/// Gauge with double-valued points (ccm_up and friends).
fn gauge(name: &str, description: &str, data_points: Vec<Value>) -> Value {
    json!({
        "name": name,
        "description": description,
        "gauge": {"dataPoints": data_points},
    })
}

/// One cumulative integer-sum data point. `asInt` sits DIRECTLY on the
/// point: protojson flattens the `oneof value` members (there is no
/// "value" key in NumberDataPoint's JSON — nesting there makes the point
/// invalid, the oneof member absent).
fn int_point(
    pairs: &[(&str, &str)],
    value: u64,
    start_unix_nano: &str,
    now_unix_nano: &str,
) -> Value {
    json!({
        "attributes": attrs(pairs),
        "startTimeUnixNano": start_unix_nano,
        "timeUnixNano": now_unix_nano,
        "asInt": value.to_string(),
    })
}

/// One double-gauge data point (`asDouble` flattened, same as `int_point`).
fn double_point(pairs: &[(&str, &str)], value: f64, now_unix_nano: &str) -> Value {
    json!({
        "attributes": attrs(pairs),
        "timeUnixNano": now_unix_nano,
        "asDouble": value,
    })
}

/// One cumulative histogram data point: 11 explicit bounds in seconds,
/// 12 delta bucket counts, double sum in seconds, int64 count as string.
fn histogram_point(
    pairs: &[(&str, &str)],
    histogram: &Histogram,
    start_unix_nano: &str,
    now_unix_nano: &str,
) -> Value {
    json!({
        "attributes": attrs(pairs),
        "startTimeUnixNano": start_unix_nano,
        "timeUnixNano": now_unix_nano,
        "count": histogram.count().to_string(),
        "sum": secs_f64(histogram.sum_ms()),
        "bucketCounts": bucket_deltas(histogram),
        "explicitBounds": crate::prometheus::BUCKETS_MS.map(secs_f64),
    })
}

fn histogram_metric(name: &str, description: &str, data_points: Vec<Value>) -> Value {
    json!({
        "name": name,
        "description": description,
        "histogram": {
            "dataPoints": data_points,
            "aggregationTemporality": TEMPORALITY_CUMULATIVE,
        },
    })
}

/// Render the full ExportMetricsServiceRequest for one push. Pure: the
/// same Snapshot shape `/metrics` renders, the OTLP way. Every one of the
/// 12 families is always present — an idle proxy still advertises them —
/// with data points only where data exists (the `render` convention).
pub(crate) fn render_otel(
    snapshot: &Snapshot,
    resource: &OtlpResource,
    start_unix_nano: &str,
    now_unix_nano: &str,
) -> Value {
    // Family builders keep each metric's data-point assembly next to its
    // wire shape. The five attempts outcomes stay disjoint exactly as in
    // the text exposition (429 subtracted back out of http_error).
    let mut metrics: Vec<Value> = Vec::with_capacity(12);

    metrics.push(gauge(
        "ccm_up",
        "Whether this CCM proxy is serving.",
        vec![double_point(&[], 1.0, now_unix_nano)],
    ));
    metrics.push(gauge(
        "ccm_process_start_time_seconds",
        "Unix time the proxy process started serving.",
        vec![double_point(
            &[],
            snapshot.process_start_seconds as f64,
            now_unix_nano,
        )],
    ));

    metrics.push(int_sum(
        "ccm_proxy_requests_total",
        "Proxied requests by target and decision outcome (success = accepted 2xx response).",
        snapshot
            .prom
            .requests
            .iter()
            .map(|((target, outcome), count)| {
                int_point(
                    &[("target", target), ("outcome", outcome)],
                    *count,
                    start_unix_nano,
                    now_unix_nano,
                )
            })
            .collect(),
    ));

    metrics.push(int_sum(
        "ccm_attempts_total",
        "Upstream attempts by model and outcome (success, http_error, rate_limited, timeout, request_error).",
        snapshot
            .models
            .iter()
            .flat_map(|(model, model_metrics)| {
                let http_error = model_metrics.http_errors.saturating_sub(model_metrics.rate_limited);
                [
                    ("success", model_metrics.successes),
                    ("http_error", http_error),
                    ("rate_limited", model_metrics.rate_limited),
                    ("timeout", model_metrics.timeouts),
                    ("request_error", model_metrics.request_errors),
                ]
                .into_iter()
                .map(move |(outcome, count)| {
                    int_point(
                        &[("model", model.as_str()), ("outcome", outcome)],
                        count,
                        start_unix_nano,
                        now_unix_nano,
                    )
                })
            })
            .collect(),
    ));

    metrics.push(histogram_metric(
        "ccm_header_latency_seconds",
        "Time to upstream response headers per model.",
        snapshot
            .prom
            .header_latency
            .iter()
            .map(|(model, histogram)| {
                histogram_point(
                    &[("model", model)],
                    histogram,
                    start_unix_nano,
                    now_unix_nano,
                )
            })
            .collect(),
    ));
    metrics.push(histogram_metric(
        "ccm_decision_duration_seconds",
        "Full routing-decision duration per target (request arrival to accepted headers or terminal failure).",
        snapshot
            .prom
            .decision_duration
            .iter()
            .map(|(target, histogram)| {
                histogram_point(&[("target", target)], histogram, start_unix_nano, now_unix_nano)
            })
            .collect(),
    ));

    metrics.push(gauge(
        "ccm_latency_ewma_ms",
        "EWMA (alpha 0.2) of upstream header latency in milliseconds; absent before the first sample.",
        snapshot
            .models
            .iter()
            .filter_map(|(model, model_metrics)| {
                model_metrics.latency_ewma_ms.map(|ewma| {
                    double_point(&[("model", model.as_str())], ewma, now_unix_nano)
                })
            })
            .collect(),
    ));

    metrics.push(gauge(
        "ccm_circuit_open",
        "1 while the model's circuit breaker is skipping it (OPEN cooldown or a HALF_OPEN probe in flight).",
        snapshot
            .circuits
            .iter()
            .map(|(model, circuit)| {
                double_point(&[("model", model.as_str())], f64::from(u8::from(circuit.open)), now_unix_nano)
            })
            .collect(),
    ));
    metrics.push(gauge(
        "ccm_circuit_consecutive_failures",
        "Consecutive upstream failures currently counted by the model's circuit breaker.",
        snapshot
            .circuits
            .iter()
            .map(|(model, circuit)| {
                double_point(
                    &[("model", model.as_str())],
                    circuit.consecutive_failures as f64,
                    now_unix_nano,
                )
            })
            .collect(),
    ));

    metrics.push(int_sum(
        "ccm_tokens_total",
        "Tokens reported by accepted responses, by model and kind (input, output, cache_read, cache_write).",
        snapshot
            .prom
            .tokens
            .iter()
            .map(|((model, kind), tokens)| {
                int_point(
                    &[("model", model.as_str()), ("kind", kind)],
                    *tokens,
                    start_unix_nano,
                    now_unix_nano,
                )
            })
            .collect(),
    ));

    metrics.push(int_sum(
        "ccm_cost_micro_usd_total",
        "Accumulated cost of accepted responses in integer micro-USD, by model (hand-entered prices; unpriced models add nothing).",
        snapshot
            .prom
            .cost_usd
            .iter()
            // Same honesty rule as the text exposition: a non-finite
            // accumulated cost emits no point rather than a fake number.
            .filter(|(_, cost)| cost.is_finite())
            .map(|(model, cost)| {
                int_point(
                    &[("model", model.as_str())],
                    (cost * 1_000_000.0).round() as u64,
                    start_unix_nano,
                    now_unix_nano,
                )
            })
            .collect(),
    ));

    metrics.push(int_sum(
        "ccm_history_dropped_total",
        "Observability records dropped (full writer queue or stopped writer) over this proxy's lifetime.",
        vec![int_point(&[], snapshot.history_dropped, start_unix_nano, now_unix_nano)],
    ));

    json!({
        "resourceMetrics": [{
            "resource": {
                "attributes": [
                    attr("service.name", resource.service_name),
                    attr("service.version", resource.service_version),
                    attr("service.instance.id", &resource.instance_id),
                ],
            },
            "scopeMetrics": [{
                "scope": {
                    "name": resource.service_name,
                    "version": resource.service_version,
                },
                "metrics": metrics,
            }],
        }],
    })
}

/// Resource identity for one push.
pub(crate) struct OtlpResource {
    pub(crate) service_name: &'static str,
    pub(crate) service_version: &'static str,
    pub(crate) instance_id: String,
}

impl OtlpResource {
    /// The proxy's resource identity: `ccm` + crate version + the stable
    /// hash of CCM_HOME.
    pub(crate) fn for_this_process() -> Self {
        Self {
            service_name: "ccm",
            service_version: env!("CARGO_PKG_VERSION"),
            instance_id: instance_id(
                &crate::config::AppConfig::home_dir()
                    .map(|path| path.display().to_string())
                    .unwrap_or_default(),
            ),
        }
    }
}

/// The dedicated push client: timeout-bounded so a stuck collector never
/// accumulates stuck tasks (the shared forwarding client has none).
pub(crate) fn push_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(3))
        .timeout(std::time::Duration::from_secs(PUSH_TIMEOUT_SECS))
        .build()
        .context("failed to build OTLP push client")
}

/// Render and POST one push. Non-2xx is an error (the caller decides how
/// loudly to complain); a failed push is never retried mid-interval —
/// cumulative temporality means the next interval re-sends everything.
pub(crate) async fn push_once(
    client: &reqwest::Client,
    endpoint: &str,
    headers: &[(String, String)],
    snapshot: &Snapshot,
    resource: &OtlpResource,
    now_ms_value: u64,
) -> Result<()> {
    let start_unix_nano = (crate::prometheus::process_start_seconds() * 1_000_000_000).to_string();
    let now_unix_nano = (now_ms_value * 1_000_000).to_string();
    let body = serde_json::to_vec(&render_otel(
        snapshot,
        resource,
        &start_unix_nano,
        &now_unix_nano,
    ))
    .context("failed to serialize OTLP payload")?;

    let mut request = client
        .post(metrics_url(endpoint))
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(body);
    for (key, value) in headers {
        request = request.header(key.as_str(), value.as_str());
    }
    let response = request.send().await.context("OTLP push request failed")?;
    let status = response.status();
    if !status.is_success() {
        anyhow::bail!("OTLP push rejected with HTTP {status}");
    }
    Ok(())
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    use crate::prometheus::{CircuitInput, PromSnapshot};
    use crate::routing::metrics::ModelMetrics;

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

    fn resource() -> OtlpResource {
        OtlpResource {
            service_name: "ccm",
            service_version: "0.5.0",
            instance_id: "0123456789abcdef".to_string(),
        }
    }

    fn rendered(snapshot: &Snapshot) -> Value {
        render_otel(
            snapshot,
            &resource(),
            "1790000000000000000",
            "1790000300000000000",
        )
    }

    fn metric<'a>(payload: &'a Value, name: &str) -> &'a Value {
        payload["resourceMetrics"][0]["scopeMetrics"][0]["metrics"]
            .as_array()
            .unwrap()
            .iter()
            .find(|metric| metric["name"] == json!(name))
            .unwrap_or_else(|| panic!("family {name} missing"))
    }

    // 1. an idle proxy still emits every one of the 12 families, and the
    //    always-present points carry real values
    #[test]
    fn empty_state_still_emits_all_families() {
        let payload = rendered(&empty_snapshot());
        let metrics = payload["resourceMetrics"][0]["scopeMetrics"][0]["metrics"]
            .as_array()
            .unwrap();
        assert_eq!(metrics.len(), 12, "exactly the 12 families");
        for name in [
            "ccm_up",
            "ccm_process_start_time_seconds",
            "ccm_proxy_requests_total",
            "ccm_attempts_total",
            "ccm_header_latency_seconds",
            "ccm_decision_duration_seconds",
            "ccm_latency_ewma_ms",
            "ccm_circuit_open",
            "ccm_circuit_consecutive_failures",
            "ccm_tokens_total",
            "ccm_cost_micro_usd_total",
            "ccm_history_dropped_total",
        ] {
            assert!(metrics.iter().any(|m| m["name"] == json!(name)), "{name}");
        }
        let up = metric(&payload, "ccm_up");
        assert_eq!(up["gauge"]["dataPoints"][0]["asDouble"], json!(1.0));
        let dropped = metric(&payload, "ccm_history_dropped_total");
        assert_eq!(dropped["sum"]["dataPoints"][0]["asInt"], json!("0"));
        assert_eq!(
            dropped["sum"]["aggregationTemporality"],
            json!(TEMPORALITY_CUMULATIVE)
        );
        assert_eq!(dropped["sum"]["isMonotonic"], json!(true));
        // resource identity + scope
        let attrs = payload["resourceMetrics"][0]["resource"]["attributes"]
            .as_array()
            .unwrap();
        assert_eq!(attrs[0]["key"], json!("service.name"));
        assert_eq!(attrs[0]["value"]["stringValue"], json!("ccm"));
        assert_eq!(attrs[2]["key"], json!("service.instance.id"));
    }

    // 2. histogram wire shape: 11 second-valued explicit bounds, 12
    //    per-bucket DELTA counts differenced from the cumulative buckets,
    //    int64 count as string, double sum in seconds
    #[test]
    fn histogram_bounds_and_delta_counts() {
        let mut histogram = Histogram::default();
        for ms in [4.0, 6.0, 600.0, 60_000.0] {
            histogram.observe_ms(ms);
        }
        // cumulative buckets: [1,2,2,2,2,2,2,3,3,3,3], count 4
        let mut snapshot = empty_snapshot();
        snapshot
            .prom
            .header_latency
            .insert("glm".to_string(), histogram);
        let payload = rendered(&snapshot);
        let point = &metric(&payload, "ccm_header_latency_seconds")["histogram"]["dataPoints"][0];

        let bounds = point["explicitBounds"].as_array().unwrap();
        assert_eq!(bounds.len(), 11, "11 finite bounds");
        assert_eq!(bounds[0], json!(0.005), "5ms -> 0.005s");
        assert_eq!(bounds[10], json!(10.0), "10000ms -> 10s");

        let counts = point["bucketCounts"].as_array().unwrap();
        assert_eq!(counts.len(), 12, "11 finite deltas + the +Inf tail");
        // deltas of [1,2,2,2,2,2,2,3,3,3,3] then 4-3
        let expected = ["1", "1", "0", "0", "0", "0", "0", "1", "0", "0", "0", "1"];
        let got: Vec<&str> = counts.iter().map(|value| value.as_str().unwrap()).collect();
        assert_eq!(got, expected, "per-bucket deltas, strings");

        assert_eq!(point["count"], json!("4"), "int64 count as string");
        assert_eq!(point["sum"], json!(60.61), "sum in seconds");
        // the model label became an attribute
        assert_eq!(
            point["attributes"][0],
            json!({"key": "model", "value": {"stringValue": "glm"}})
        );
        // temporality on the histogram too
        assert_eq!(
            metric(&payload, "ccm_header_latency_seconds")["histogram"]["aggregationTemporality"],
            json!(TEMPORALITY_CUMULATIVE)
        );
    }

    // 3. int sums map labels to attributes and keep the disjoint
    //    five-outcome attempts split (429 subtracted from http_error)
    #[test]
    fn sums_map_labels_and_split_attempts() {
        let mut snapshot = empty_snapshot();
        snapshot
            .prom
            .requests
            .insert(("coding-route".to_string(), "success".to_string()), 2);
        snapshot.models.push((
            "glm".to_string(),
            ModelMetrics {
                attempts: 6,
                successes: 2,
                http_errors: 3,
                rate_limited: 1,
                timeouts: 1,
                ..ModelMetrics::default()
            },
        ));
        snapshot
            .prom
            .tokens
            .insert(("glm".to_string(), "input"), 100);
        snapshot.prom.cost_usd.insert("glm".to_string(), 0.0000015);

        let payload = rendered(&snapshot);
        let requests = metric(&payload, "ccm_proxy_requests_total")["sum"]["dataPoints"][0].clone();
        assert_eq!(
            requests["attributes"],
            json!([
                {"key": "target", "value": {"stringValue": "coding-route"}},
                {"key": "outcome", "value": {"stringValue": "success"}},
            ])
        );
        assert_eq!(requests["asInt"], json!("2"));

        let attempts = metric(&payload, "ccm_attempts_total")["sum"]["dataPoints"]
            .as_array()
            .unwrap();
        // five disjoint outcomes; a 429 counts once
        let by_outcome = |outcome: &str| {
            attempts
                .iter()
                .find(|point| point["attributes"][1]["value"]["stringValue"] == json!(outcome))
                .unwrap_or_else(|| panic!("{outcome} point missing"))["asInt"]
                .clone()
        };
        assert_eq!(by_outcome("success"), json!("2"));
        assert_eq!(by_outcome("http_error"), json!("2"));
        assert_eq!(by_outcome("rate_limited"), json!("1"));
        assert_eq!(by_outcome("timeout"), json!("1"));
        assert_eq!(by_outcome("request_error"), json!("0"));

        let tokens = metric(&payload, "ccm_tokens_total")["sum"]["dataPoints"][0].clone();
        assert_eq!(tokens["asInt"], json!("100"));
        let cost = metric(&payload, "ccm_cost_micro_usd_total")["sum"]["dataPoints"][0].clone();
        assert_eq!(cost["asInt"], json!("2"), "1.5 rounds to 2");
    }

    // 4. gauges: EWMA absent until first sample; circuit gauges carry the
    //    open bit and failure count; non-finite cost emits nothing
    #[test]
    fn gauges_and_cost_honesty() {
        let mut snapshot = empty_snapshot();
        snapshot.models.push((
            "glm".to_string(),
            ModelMetrics {
                latency_ewma_ms: None,
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
        snapshot.circuits.push((
            "glm".to_string(),
            CircuitInput {
                open: true,
                consecutive_failures: 3,
            },
        ));
        snapshot
            .prom
            .cost_usd
            .insert("absurd".to_string(), f64::INFINITY);
        snapshot.prom.cost_usd.insert("glm".to_string(), 0.000_412);

        let payload = rendered(&snapshot);
        let ewma = metric(&payload, "ccm_latency_ewma_ms")["gauge"]["dataPoints"]
            .as_array()
            .unwrap();
        assert_eq!(ewma.len(), 1, "unsampled EWMA stays absent");
        assert_eq!(ewma[0]["asDouble"], json!(204.5));

        let open = metric(&payload, "ccm_circuit_open")["gauge"]["dataPoints"][0].clone();
        assert_eq!(open["asDouble"], json!(1.0));
        let failures =
            metric(&payload, "ccm_circuit_consecutive_failures")["gauge"]["dataPoints"][0].clone();
        assert_eq!(failures["asDouble"], json!(3.0));

        let cost = metric(&payload, "ccm_cost_micro_usd_total")["sum"]["dataPoints"]
            .as_array()
            .unwrap();
        assert_eq!(cost.len(), 1, "non-finite cost emits no point");
        assert_eq!(cost[0]["asInt"], json!("412"));
    }

    // 5. endpoint joining: /v1/metrics appended exactly once
    #[test]
    fn metrics_url_appends_path_exactly_once() {
        assert_eq!(
            metrics_url("http://localhost:4318"),
            "http://localhost:4318/v1/metrics"
        );
        assert_eq!(
            metrics_url("http://localhost:4318/"),
            "http://localhost:4318/v1/metrics"
        );
        assert_eq!(
            metrics_url("http://collector:4318/v1/metrics"),
            "http://collector:4318/v1/metrics"
        );
        assert_eq!(
            metrics_url("http://collector:4318/v1/metrics/"),
            "http://collector:4318/v1/metrics"
        );
    }

    // 6. header env parsing: standard pairs, whitespace tolerated,
    //    malformed entries skipped
    #[test]
    fn headers_from_env_parses_standard_pairs() {
        let headers = headers_from_env("api-key=secret, tenant=acme ,bad-no-equals, =x,ok=");
        let flat: Vec<(String, String)> = headers
            .into_iter()
            .filter(|(key, _)| key == "api-key" || key == "tenant" || key == "ok")
            .collect();
        assert_eq!(
            flat,
            vec![
                ("api-key".to_string(), "secret".to_string()),
                ("tenant".to_string(), "acme".to_string()),
                ("ok".to_string(), String::new()),
            ]
        );
    }

    // 7. instance id: stable, 16 hex chars, differs across homes
    #[test]
    fn instance_id_is_stable_and_distinct() {
        let a = instance_id(r"C:\Users\x\.ccm");
        assert_eq!(a, instance_id(r"C:\Users\x\.ccm"));
        assert_eq!(a.len(), 16);
        assert!(a.chars().all(|ch| ch.is_ascii_hexdigit()));
        assert_ne!(a, instance_id(r"C:\Users\y\.ccm"));
    }

    // 8. credential boundary: the payload carries no credential-looking
    //    material even when the push headers do
    #[test]
    fn payload_carries_no_credential_material() {
        let mut snapshot = empty_snapshot();
        snapshot
            .prom
            .requests
            .insert(("route".to_string(), "success".to_string()), 1);
        let text = serde_json::to_string(&rendered(&snapshot)).unwrap();
        for secret in ["api-key", "secret", "Bearer", "authorization", "api_key"] {
            assert!(
                !text
                    .to_ascii_lowercase()
                    .contains(&secret.to_ascii_lowercase()),
                "payload mentions {secret}"
            );
        }
    }

    // 9. the wire half: a live collector receives a valid
    //    ExportMetricsServiceRequest POST (application/json, all 12
    //    families, env-plumbed headers riding the REQUEST only); 200 → Ok;
    //    a dead port and a non-2xx are Errors (the loop's
    //    warn-and-continue contract); and a push after a failure re-sends
    //    the full cumulative state — nothing is lost by not retrying.
    #[tokio::test]
    async fn push_once_covers_the_wire_contract() {
        use std::sync::{Arc, Mutex};

        use axum::{body::Body, extract::Request, http::StatusCode, routing::post, Router};

        /// (content-type, body) of each captured POST.
        type Captured = Vec<(Option<String>, String)>;
        let seen: Arc<Mutex<Captured>> = Arc::new(Mutex::new(Vec::new()));
        let seen_route = seen.clone();
        let app = Router::new().route(
            "/v1/metrics",
            post(move |request: Request<Body>| async move {
                let content_type = request
                    .headers()
                    .get("content-type")
                    .and_then(|value| value.to_str().ok())
                    .map(str::to_string);
                let bytes = axum::body::to_bytes(request.into_body(), 8 * 1024 * 1024)
                    .await
                    .unwrap();
                seen_route
                    .lock()
                    .unwrap()
                    .push((content_type, String::from_utf8(bytes.to_vec()).unwrap()));
                StatusCode::OK
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let rejecting =
            Router::new().route("/v1/metrics", post(|| async { StatusCode::FORBIDDEN }));
        let rejecting_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let rejecting_addr = rejecting_listener.local_addr().unwrap();
        let rejecting_task =
            tokio::spawn(async move { axum::serve(rejecting_listener, rejecting).await.unwrap() });

        let client = push_client().unwrap();
        let mut snapshot = empty_snapshot();
        snapshot
            .prom
            .requests
            .insert(("coding-route".to_string(), "success".to_string()), 3);
        snapshot.models.push((
            "glm".to_string(),
            ModelMetrics {
                attempts: 3,
                successes: 2,
                rate_limited: 1,
                latency_ewma_ms: Some(120.0),
                ..ModelMetrics::default()
            },
        ));
        let mut histogram = Histogram::default();
        histogram.observe_ms(120.0);
        histogram.observe_ms(6000.0);
        snapshot
            .prom
            .header_latency
            .insert("glm".to_string(), histogram);

        // success: base URL joined, JSON content type, full payload
        push_once(
            &client,
            &format!("http://{addr}"),
            &[("x-scope-tenant".to_string(), "acme".to_string())],
            &snapshot,
            &resource(),
            1_790_000_300_000,
        )
        .await
        .unwrap();
        let captured = seen.lock().unwrap().clone();
        assert_eq!(captured.len(), 1);
        let (content_type, body) = &captured[0];
        assert_eq!(content_type.as_deref(), Some("application/json"));
        let payload: Value = serde_json::from_str(body).unwrap();
        let metrics = payload["resourceMetrics"][0]["scopeMetrics"][0]["metrics"]
            .as_array()
            .unwrap();
        assert_eq!(metrics.len(), 12, "all families arrive over the wire");
        assert_eq!(
            payload["resourceMetrics"][0]["scopeMetrics"][0]["scope"]["name"],
            json!("ccm")
        );
        assert_eq!(
            payload["resourceMetrics"][0]["scopeMetrics"][0]["metrics"][0]["gauge"]["dataPoints"]
                [0]["timeUnixNano"],
            json!("1790000300000000000"),
            "now_ms rides as fixed64 decimal string"
        );
        assert!(
            !body.contains("acme") && !body.contains("x-scope-tenant"),
            "request headers never leak into the payload"
        );

        // failure isolation: a dead port and a collector refusal are both
        // Errors — never panics, never retries
        push_once(
            &client,
            "http://127.0.0.1:1",
            &[],
            &snapshot,
            &resource(),
            1,
        )
        .await
        .unwrap_err();
        push_once(
            &client,
            &format!("http://{rejecting_addr}"),
            &[],
            &snapshot,
            &resource(),
            1,
        )
        .await
        .unwrap_err();

        // cumulative temporality: the next successful push after those
        // failures re-sends the FULL state — the counts are unchanged
        push_once(
            &client,
            &format!("http://{addr}"),
            &[],
            &snapshot,
            &resource(),
            1_790_000_360_000,
        )
        .await
        .unwrap();
        let captured = seen.lock().unwrap().clone();
        assert_eq!(captured.len(), 2, "loop continued past the failures");
        let payload: Value = serde_json::from_str(&captured[1].1).unwrap();
        let requests = payload["resourceMetrics"][0]["scopeMetrics"][0]["metrics"]
            .as_array()
            .unwrap()
            .iter()
            .find(|metric| metric["name"] == json!("ccm_proxy_requests_total"))
            .unwrap()["sum"]["dataPoints"][0]["asInt"]
            .clone();
        assert_eq!(requests, json!("3"), "nothing was lost by not retrying");

        task.abort();
        rejecting_task.abort();
    }
}
