use std::{
    collections::{HashMap, VecDeque},
    net::SocketAddr,
    sync::{atomic::AtomicU64, Arc},
    time::{Duration, Instant},
};

use anyhow::{bail, Context, Result};
use axum::{
    body::{Body, Bytes},
    extract::State,
    http::{header, HeaderMap, Method, Request, Response, StatusCode},
    response::IntoResponse,
};
use reqwest::Client;
use serde_json::Value;
use tokio::{
    sync::RwLock,
    time::{sleep, timeout},
};

use crate::{
    config::AppConfig,
    control::api::control_router,
    credential,
    route::RoutePolicy,
    routing::circuit::{
        circuit_admit, circuit_decision_name, circuit_failure, circuit_success,
        release_half_open_probe, CircuitDecision, CircuitState,
    },
    routing::decision::{
        build_routing_decision, record_attempt_started, record_http_response, record_request_error,
        record_timeout, store_decision, trace_attempt, AttemptTrace, DecisionAttempt,
        RoutingDecision, DECISION_CAPACITY, TRACE_CAPACITY,
    },
    routing::metrics::ModelMetrics,
    routing::select::select_candidates,
    state::AppState,
};

#[derive(Clone)]
pub(crate) struct ProxyState {
    pub(crate) client: Client,
    pub(crate) target: Arc<RwLock<String>>,
    pub(crate) traces: Arc<RwLock<VecDeque<AttemptTrace>>>,
    pub(crate) circuits: Arc<RwLock<HashMap<String, CircuitState>>>,
    pub(crate) metrics: Arc<RwLock<HashMap<String, ModelMetrics>>>,
    pub(crate) decisions: Arc<RwLock<VecDeque<RoutingDecision>>>,
    pub(crate) decision_seq: Arc<AtomicU64>,
}

pub async fn serve(bind: &str) -> Result<()> {
    let addr: SocketAddr = bind
        .parse()
        .with_context(|| format!("invalid bind address `{bind}`"))?;
    ensure_loopback(addr)?;

    let config = AppConfig::load().context("failed to load CCM config")?;
    let persisted = AppState::load_or_migrate(&config).context("failed to load CCM state")?;
    let target = persisted
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
        decisions: Arc::new(RwLock::new(VecDeque::with_capacity(DECISION_CAPACITY))),
        decision_seq: Arc::new(AtomicU64::new(1)),
    };

    let app = control_router(state);

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("failed to bind {addr}"))?;

    println!("CCM proxy listening on http://{addr}");
    println!("Claude Code base URL: http://{addr}");
    println!("Runtime switch: `ccm switch <model-or-profile-or-route>`.");

    axum::serve(listener, app)
        .await
        .context("proxy server failed")
}

// The control API and the credential-injecting proxy are unauthenticated, so
// v0.3 refuses to expose them beyond the local machine. Remote/LAN binding
// needs an authentication design first (post-v0.3).
fn ensure_loopback(addr: SocketAddr) -> Result<()> {
    if !addr.ip().is_loopback() {
        bail!("refusing to bind non-loopback address {addr}: the CCM control API is unauthenticated, remote binding is not supported in v0.3");
    }
    Ok(())
}

pub(crate) async fn forward_messages(
    State(state): State<ProxyState>,
    request: Request<Body>,
) -> impl IntoResponse {
    match forward(state, request).await {
        Ok(response) => response,
        Err(err) => (StatusCode::BAD_GATEWAY, format!("CCM proxy error: {err:#}")).into_response(),
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
        configured_candidates.clone(),
        &route.policy.selection,
        &route.policy.weights,
    )
    .await;
    let mut decision = build_routing_decision(
        &state,
        &config,
        &route.policy,
        &target,
        &configured_candidates,
        &candidates,
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

        let circuit_decision =
            circuit_admit(&state, candidate, &route.policy.circuit_breaker).await;
        match circuit_decision {
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
                decision.attempts.push(DecisionAttempt {
                    attempt: actual_attempts + 1,
                    model: candidate.clone(),
                    circuit: circuit_decision_name(circuit_decision),
                    result: result.clone(),
                    fallback: index + 1 < candidates.len(),
                });
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
                decision.attempts.push(DecisionAttempt {
                    attempt: actual_attempts + 1,
                    model: candidate.clone(),
                    circuit: circuit_decision_name(circuit_decision),
                    result: result.clone(),
                    fallback: index + 1 < candidates.len(),
                });
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

        let resolved = (|| {
            let model = config.models.get(candidate).with_context(|| {
                format!("route candidate model `{candidate}` is not configured")
            })?;
            let provider = config
                .providers
                .get(&model.provider)
                .with_context(|| format!("provider `{}` is not configured", model.provider))?;
            let token = credential::get(&model.provider)?;
            let body = rewrite_model(bytes.clone(), &model.model_id)?;
            Ok((provider, token, body))
        })();
        let (provider, token, body) = match resolved {
            Ok(resolved) => resolved,
            Err(err) => {
                // Resolve failures happen before any upstream attempt: release the
                // HALF_OPEN probe slot instead of wedging it, and record the routing
                // decision so the request is still explainable.
                if matches!(circuit_decision, CircuitDecision::HalfOpen) {
                    release_half_open_probe(&state, candidate).await;
                }
                let result = format!("resolve error: {err:#}");
                trace_attempt(&state, &target, attempt, candidate, &result, false).await;
                decision.attempts.push(DecisionAttempt {
                    attempt,
                    model: candidate.clone(),
                    circuit: circuit_decision_name(circuit_decision),
                    result: result.clone(),
                    fallback: false,
                });
                decision.outcome = result;
                store_decision(&state, decision).await;
                return Err(err);
            }
        };
        let upstream = format!("{}/v1/messages", provider.base_url.trim_end_matches('/'));

        let mut builder = state.client.post(upstream).body(body);
        builder = copy_request_headers(builder, &parts.headers);
        builder = provider.apply_auth(builder, &token);

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
                decision.attempts.push(DecisionAttempt {
                    attempt,
                    model: candidate.clone(),
                    circuit: circuit_decision_name(circuit_decision),
                    result: result.clone(),
                    fallback: can_fallback,
                });
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
                decision.attempts.push(DecisionAttempt {
                    attempt,
                    model: candidate.clone(),
                    circuit: circuit_decision_name(circuit_decision),
                    result: result.clone(),
                    fallback: can_fallback,
                });
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
            decision.attempts.push(DecisionAttempt {
                attempt,
                model: candidate.clone(),
                circuit: circuit_decision_name(circuit_decision),
                result: result.clone(),
                fallback: can_fallback,
            });
            failures.push(format!("{candidate}: {result}"));
            if can_fallback {
                apply_backoff(route.policy.backoff_ms).await;
                continue;
            }
            decision.selected = Some(candidate.clone());
            decision.outcome = result;
            store_decision(&state, decision).await;
            return proxy_response(upstream_response);
        }

        circuit_success(&state, candidate, &route.policy.circuit_breaker).await;
        let result = format!("HTTP {status}");
        trace_attempt(&state, &target, attempt, candidate, &result, false).await;
        decision.attempts.push(DecisionAttempt {
            attempt,
            model: candidate.clone(),
            circuit: circuit_decision_name(circuit_decision),
            result: result.clone(),
            fallback: false,
        });
        decision.selected = Some(candidate.clone());
        decision.outcome = result;
        store_decision(&state, decision).await;
        return proxy_response(upstream_response);
    }

    decision.outcome = if failures.is_empty() {
        "no candidate admitted or attempt budget exhausted".to_string()
    } else {
        format!("failed: {}", failures.join("; "))
    };
    store_decision(&state, decision).await;

    bail!(
        "all available route candidates failed for `{target}`: {}",
        failures.join("; ")
    )
}

fn elapsed_ms(started: Instant) -> f64 {
    started.elapsed().as_secs_f64() * 1000.0
}

async fn apply_backoff(backoff_ms: u64) {
    if backoff_ms > 0 {
        sleep(Duration::from_millis(backoff_ms)).await;
    }
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
    let mut value: Value =
        serde_json::from_slice(&bytes).context("request body is not valid JSON")?;
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
    use crate::control::api::status_view;
    use crate::route::SelectionWeights;
    use crate::routing::decision::now_ms;
    use crate::routing::metrics::{health_score, success_rate, update_latency_ewma};
    use crate::routing::select::{candidate_health_rank, weighted_score};
    use axum::{routing::post, Router};

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
    fn rejects_non_loopback_bind_addresses() {
        for bind in ["127.0.0.1:13521", "127.8.8.4:0", "[::1]:13521"] {
            let addr: SocketAddr = bind.parse().unwrap();
            ensure_loopback(addr).unwrap();
        }
        for bind in ["0.0.0.0:13521", "192.168.1.10:13521", "[::]:13521"] {
            let addr: SocketAddr = bind.parse().unwrap();
            let error = ensure_loopback(addr).unwrap_err();
            assert!(error.to_string().contains("loopback"));
        }
    }

    #[tokio::test]
    async fn serve_refuses_non_loopback_before_loading_config() {
        let error = serve("0.0.0.0:13521").await.unwrap_err();
        assert!(error.to_string().contains("loopback"));
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

    #[derive(Clone)]
    enum MockBehavior {
        OkStream,
        Status(StatusCode),
        DelayOk(u64),
    }

    #[derive(Clone)]
    struct CapturedRequest {
        headers: HeaderMap,
        body: Value,
    }

    #[derive(Clone)]
    struct MockProviderState {
        behavior: Arc<RwLock<MockBehavior>>,
        requests: Arc<std::sync::Mutex<Vec<CapturedRequest>>>,
    }

    async fn mock_messages(
        State(state): State<MockProviderState>,
        headers: HeaderMap,
        body: Bytes,
    ) -> Response<Body> {
        let body: Value = serde_json::from_slice(&body).unwrap();
        state
            .requests
            .lock()
            .unwrap()
            .push(CapturedRequest { headers, body });

        match state.behavior.read().await.clone() {
            MockBehavior::OkStream => stream_ok(),
            MockBehavior::Status(status) => Response::builder()
                .status(status)
                .body(Body::from(format!("mock {status}")))
                .unwrap(),
            MockBehavior::DelayOk(delay_ms) => {
                sleep(Duration::from_millis(delay_ms)).await;
                stream_ok()
            }
        }
    }

    fn stream_ok() -> Response<Body> {
        Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "text/event-stream")
            .body(Body::from(
                "event: message_start\ndata: {\"type\":\"message_start\"}\n\n",
            ))
            .unwrap()
    }

    async fn spawn_mock(
        behavior: MockBehavior,
    ) -> (SocketAddr, MockProviderState, tokio::task::JoinHandle<()>) {
        let state = MockProviderState {
            behavior: Arc::new(RwLock::new(behavior)),
            requests: Arc::new(std::sync::Mutex::new(Vec::new())),
        };
        let app = Router::new()
            .route("/v1/messages", post(mock_messages))
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (addr, state, handle)
    }

    fn integration_proxy_state() -> ProxyState {
        ProxyState {
            client: Client::new(),
            target: Arc::new(RwLock::new("test-route".to_string())),
            traces: Arc::new(RwLock::new(VecDeque::with_capacity(TRACE_CAPACITY))),
            circuits: Arc::new(RwLock::new(HashMap::new())),
            metrics: Arc::new(RwLock::new(HashMap::new())),
            decisions: Arc::new(RwLock::new(VecDeque::with_capacity(DECISION_CAPACITY))),
            decision_seq: Arc::new(AtomicU64::new(1)),
        }
    }

    fn integration_request() -> Request<Body> {
        Request::builder()
            .method(Method::POST)
            .uri("/v1/messages")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                r#"{"model":"ccm","max_tokens":16,"messages":[{"role":"user","content":"ping"}]}"#,
            ))
            .unwrap()
    }

    fn write_integration_config(
        root: &std::path::Path,
        primary_url: &str,
        fallback_url: &str,
        selection: &str,
        header_timeout_ms: u64,
        failure_threshold: usize,
        open_ms: u64,
    ) {
        std::fs::create_dir_all(root).unwrap();
        let raw = format!(
            r#"
[providers.primary]
kind = "anthropic-compatible"
base_url = "{primary_url}"
auth = "x-api-key"

[providers.fallback]
kind = "anthropic-compatible"
base_url = "{fallback_url}"
auth = "bearer"

[models.primary]
provider = "primary"
model_id = "upstream-primary"

[models.primary.routing]
cost_weight = 10.0
quality_weight = 0.2

[models.fallback]
provider = "fallback"
model_id = "upstream-fallback"

[models.fallback.routing]
cost_weight = 0.1
quality_weight = 0.9

[routes.test-route]
primary = "primary"
fallback = ["fallback"]

[routes.test-route.policy]
selection = "{selection}"
header_timeout_ms = {header_timeout_ms}
fallback_on = [429, 503]
max_attempts = 2
backoff_ms = 0

[routes.test-route.policy.weights]
reliability = 0.0
latency = 0.0
cost = 0.5
quality = 0.5

[routes.test-route.policy.circuit_breaker]
enabled = true
failure_threshold = {failure_threshold}
open_ms = {open_ms}
"#,
        );
        std::fs::write(root.join("config.toml"), raw).unwrap();
    }

    async fn body_text(response: Response<Body>) -> String {
        let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    #[tokio::test]
    async fn mock_provider_integration_covers_v03_routing_contract() {
        let root = std::env::temp_dir().join(format!(
            "ccm-mock-integration-{}-{}",
            std::process::id(),
            now_ms()
        ));
        std::env::set_var("CCM_HOME", &root);
        std::env::set_var("CCM_PRIMARY_API_KEY", "primary-secret");
        std::env::set_var("CCM_FALLBACK_API_KEY", "fallback-secret");

        let (primary_addr, primary, primary_task) = spawn_mock(MockBehavior::OkStream).await;
        let (fallback_addr, fallback, fallback_task) = spawn_mock(MockBehavior::OkStream).await;
        let primary_url = format!("http://{primary_addr}");
        let fallback_url = format!("http://{fallback_addr}");
        let state = integration_proxy_state();

        // 200 streaming + x-api-key + model rewrite.
        write_integration_config(&root, &primary_url, &fallback_url, "ordered", 250, 3, 50);
        let response = forward(state.clone(), integration_request()).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert!(body_text(response).await.contains("message_start"));
        {
            let requests = primary.requests.lock().unwrap();
            let captured = requests.last().unwrap();
            assert_eq!(
                captured.headers.get("x-api-key").unwrap().to_str().unwrap(),
                "primary-secret"
            );
            assert!(captured.headers.get(header::AUTHORIZATION).is_none());
            assert_eq!(captured.body["model"], "upstream-primary");
        }

        // 429 fallback + bearer auth + decision trace.
        *primary.behavior.write().await = MockBehavior::Status(StatusCode::TOO_MANY_REQUESTS);
        let response = forward(state.clone(), integration_request()).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert!(body_text(response).await.contains("message_start"));
        {
            let requests = fallback.requests.lock().unwrap();
            let captured = requests.last().unwrap();
            assert_eq!(
                captured
                    .headers
                    .get(header::AUTHORIZATION)
                    .unwrap()
                    .to_str()
                    .unwrap(),
                "Bearer fallback-secret"
            );
            assert!(captured.headers.get("x-api-key").is_none());
            assert_eq!(captured.body["model"], "upstream-fallback");
        }
        {
            let decisions = state.decisions.read().await;
            let decision = decisions.back().unwrap();
            assert_eq!(decision.selected.as_deref(), Some("fallback"));
            assert_eq!(decision.attempts.len(), 2);
            assert!(decision.attempts[0].result.contains("429"));
            assert!(decision.attempts[0].fallback);
        }

        // Header timeout falls back.
        state.circuits.write().await.clear();
        *primary.behavior.write().await = MockBehavior::DelayOk(100);
        write_integration_config(&root, &primary_url, &fallback_url, "ordered", 20, 3, 50);
        let response = forward(state.clone(), integration_request()).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        {
            let decisions = state.decisions.read().await;
            let decision = decisions.back().unwrap();
            assert_eq!(decision.selected.as_deref(), Some("fallback"));
            assert!(decision.attempts[0].result.contains("timeout"));
        }

        // 503 opens the circuit; next request skips primary; cooldown allows HALF_OPEN recovery.
        state.circuits.write().await.clear();
        *primary.behavior.write().await = MockBehavior::Status(StatusCode::SERVICE_UNAVAILABLE);
        write_integration_config(&root, &primary_url, &fallback_url, "ordered", 250, 1, 30);
        let response = forward(state.clone(), integration_request()).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let primary_count_after_open = primary.requests.lock().unwrap().len();

        let response = forward(state.clone(), integration_request()).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            primary.requests.lock().unwrap().len(),
            primary_count_after_open
        );
        {
            let decisions = state.decisions.read().await;
            let decision = decisions.back().unwrap();
            assert!(decision.attempts[0].result.contains("circuit OPEN"));
            assert_eq!(decision.selected.as_deref(), Some("fallback"));
        }

        sleep(Duration::from_millis(40)).await;
        *primary.behavior.write().await = MockBehavior::OkStream;
        let response = forward(state.clone(), integration_request()).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        {
            let decisions = state.decisions.read().await;
            let decision = decisions.back().unwrap();
            assert_eq!(decision.selected.as_deref(), Some("primary"));
            assert_eq!(decision.attempts[0].circuit, "HALF_OPEN");
        }
        {
            let circuits = state.circuits.read().await;
            let primary_circuit = circuits.get("primary").unwrap();
            assert!(primary_circuit.open_until_ms.is_none());
            assert_eq!(primary_circuit.consecutive_failures, 0);
        }

        // A resolve failure during HALF_OPEN must release the probe slot instead of
        // wedging the model out of routing until restart.
        state.circuits.write().await.clear();
        *primary.behavior.write().await = MockBehavior::Status(StatusCode::SERVICE_UNAVAILABLE);
        write_integration_config(&root, &primary_url, &fallback_url, "ordered", 250, 1, 30);
        let response = forward(state.clone(), integration_request()).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        sleep(Duration::from_millis(40)).await;
        *primary.behavior.write().await = MockBehavior::OkStream;
        let malformed = Request::builder()
            .method(Method::POST)
            .uri("/v1/messages")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from("not json"))
            .unwrap();
        assert!(forward(state.clone(), malformed).await.is_err());
        {
            let decisions = state.decisions.read().await;
            let decision = decisions.back().unwrap();
            assert!(decision.outcome.contains("resolve error"));
        }

        let response = forward(state.clone(), integration_request()).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        {
            let decisions = state.decisions.read().await;
            let decision = decisions.back().unwrap();
            assert_eq!(decision.attempts[0].circuit, "HALF_OPEN");
            assert_eq!(decision.selected.as_deref(), Some("primary"));
        }

        // Weighted routing uses static cost/quality metadata and selects fallback first.
        state.circuits.write().await.clear();
        write_integration_config(&root, &primary_url, &fallback_url, "weighted", 250, 3, 50);
        let primary_before = primary.requests.lock().unwrap().len();
        let fallback_before = fallback.requests.lock().unwrap().len();
        let response = forward(state.clone(), integration_request()).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(primary.requests.lock().unwrap().len(), primary_before);
        assert_eq!(fallback.requests.lock().unwrap().len(), fallback_before + 1);
        {
            let decisions = state.decisions.read().await;
            let decision = decisions.back().unwrap();
            assert_eq!(decision.selection, "weighted");
            assert_eq!(decision.ranked_candidates[0].model, "fallback");
            assert_eq!(decision.selected.as_deref(), Some("fallback"));
        }

        primary_task.abort();
        fallback_task.abort();
        std::env::remove_var("CCM_HOME");
        std::env::remove_var("CCM_PRIMARY_API_KEY");
        std::env::remove_var("CCM_FALLBACK_API_KEY");
        let _ = std::fs::remove_dir_all(root);
    }
}
