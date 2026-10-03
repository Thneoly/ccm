use std::{
    collections::{HashMap, VecDeque},
    convert::Infallible,
    net::SocketAddr,
    pin::Pin,
    sync::{atomic::AtomicU64, Arc},
    task::{Context as TaskContext, Poll},
    time::{Duration, Instant},
};

use anyhow::{bail, Context, Result};
use axum::{
    body::{Body, Bytes},
    extract::State,
    http::{header, HeaderMap, Method, Request, Response, StatusCode},
    response::IntoResponse,
};
use futures_core::Stream;
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
    provider::ProviderKind,
    route::RoutePolicy,
    routing::circuit::{
        circuit_admit, circuit_decision_name, circuit_failure, circuit_success,
        release_half_open_probe, CircuitDecision, CircuitState,
    },
    routing::decision::{
        build_routing_decision, now_ms, record_attempt_started, record_http_response,
        record_request_error, record_timeout, store_decision, trace_attempt, AttemptTrace,
        DecisionAttempt, RoutingDecision, DECISION_CAPACITY, TRACE_CAPACITY,
    },
    routing::metrics::ModelMetrics,
    routing::select::select_candidates,
    state::AppState,
    translate::{self, SseEvent, SseTranslator},
};

#[derive(Clone)]
pub(crate) struct ProxyState {
    pub(crate) client: Client,
    pub(crate) target: Arc<RwLock<String>>,
    /// Per-client runtime targets (v0.4 M3): in-memory only, never written to
    /// any file, cleared on restart (invariant 8). Entries are created ONLY by
    /// a scoped switch — an unknown client id follows the global target and
    /// never materializes an entry.
    pub(crate) clients: Arc<RwLock<HashMap<String, ClientEntry>>>,
    pub(crate) traces: Arc<RwLock<VecDeque<AttemptTrace>>>,
    pub(crate) circuits: Arc<RwLock<HashMap<String, CircuitState>>>,
    pub(crate) metrics: Arc<RwLock<HashMap<String, ModelMetrics>>>,
    pub(crate) decisions: Arc<RwLock<VecDeque<RoutingDecision>>>,
    pub(crate) decision_seq: Arc<AtomicU64>,
}

/// One client's runtime switch state. `requests` / `last_seen_ms` are usage
/// counters bumped on every request that resolves through the entry.
#[derive(Clone, Default)]
pub(crate) struct ClientEntry {
    pub(crate) target: String,
    pub(crate) requests: u64,
    pub(crate) last_seen_ms: u64,
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
        clients: Arc::new(RwLock::new(HashMap::new())),
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

// ===========================================================================
// client identity + per-client target resolution (v0.4 M3)
// ===========================================================================

/// Local identity header, injected by the launcher; never forwarded upstream.
const CLIENT_HEADER: &str = "x-ccm-client";
/// Bearer placeholder prefix carrying a client id (the launcher's fallback
/// channel when the client cannot set custom headers).
const CLIENT_TOKEN_PREFIX: &str = "ccm-local-";

/// Client ids are `[A-Za-z0-9._-]{1,64}` — the same charset the scoped
/// switch accepts. Shared with the control API's switch validation.
pub(crate) fn valid_client_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

/// Error text for an out-of-charset client id, shared by the proxy-side
/// switch validation and the CLI-side early check so both report identically
/// by construction.
pub(crate) fn invalid_client_id_message(id: &str) -> String {
    format!("invalid client id `{id}`: must be 1-64 characters of [A-Za-z0-9._-]")
}

/// Extract the requesting client id: the `x-ccm-client` header first, then the
/// `Authorization: Bearer ccm-local-<id>` placeholder token. The bare v0.3
/// placeholder `ccm-local` carries no id. A real bearer credential yields no
/// id and is never parsed or logged beyond the prefix checks. Anything out of
/// charset is treated as "no id" — the request still routes, globally.
fn client_id_from_headers(headers: &HeaderMap) -> Option<String> {
    if let Some(value) = headers.get(CLIENT_HEADER) {
        // The header channel is authoritative: an unusable value means no id,
        // not a silent fallthrough to the token channel.
        return value
            .to_str()
            .ok()
            .filter(|id| valid_client_id(id))
            .map(str::to_string);
    }
    let authorization = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let token = authorization.strip_prefix("Bearer ")?;
    let id = token.strip_prefix(CLIENT_TOKEN_PREFIX)?;
    valid_client_id(id).then(|| id.to_string())
}

/// Effective target for one request: a switched client's entry target (with
/// its usage counters bumped), or the global target. An id WITHOUT an entry
/// follows the global target and never creates an entry — only a scoped
/// switch does. No id is byte-identical to v0.3.
async fn resolve_request_target(state: &ProxyState, client: Option<&str>) -> String {
    let global = state.target.read().await.clone();
    match client {
        Some(id) => {
            let mut clients = state.clients.write().await;
            match clients.get_mut(id) {
                Some(entry) => {
                    entry.requests += 1;
                    entry.last_seen_ms = now_ms();
                    entry.target.clone()
                }
                None => global,
            }
        }
        None => global,
    }
}

async fn forward(state: ProxyState, request: Request<Body>) -> Result<Response<Body>> {
    if request.method() != Method::POST {
        return Ok((StatusCode::METHOD_NOT_ALLOWED, "POST required").into_response());
    }

    // Client identity first (header channel, then the placeholder token): it
    // decides which runtime target this request resolves through. No id means
    // the global target, exactly as v0.3.
    let client_id = client_id_from_headers(request.headers());
    let target = resolve_request_target(&state, client_id.as_deref()).await;
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
        client_id.as_deref(),
        &configured_candidates,
        &candidates,
    )
    .await;

    let (parts, body) = request.into_parts();
    let bytes = axum::body::to_bytes(body, 16 * 1024 * 1024)
        .await
        .context("failed to read request body")?;

    // Request-level stream flag: parsed once, and only when a candidate can
    // actually require an openai-compatible translation — anthropic-only
    // routes (the v0.3 hot path) skip the extra full-body parse entirely. A
    // malformed body simply yields false here; the per-candidate preparation
    // will fail on it anyway.
    let may_translate = candidates.iter().any(|candidate| {
        config
            .models
            .get(candidate)
            .and_then(|model| config.providers.get(&model.provider))
            .is_some_and(|provider| provider.kind == ProviderKind::OpenAICompatible)
    });
    let client_wants_stream = may_translate
        && serde_json::from_slice::<Value>(&bytes)
            .map(|value| translate::request_wants_stream(&value))
            .unwrap_or(false);

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
                    client_id.as_deref(),
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
                    client_id.as_deref(),
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
            let body = prepare_request_body(&bytes, &model.model_id, provider.kind)?;
            Ok((provider, token, body, model.model_id.clone()))
        })();
        let (provider, token, body, model_id) = match resolved {
            Ok(resolved) => resolved,
            Err(err) => {
                // Resolve failures happen before any upstream attempt: release the
                // HALF_OPEN probe slot instead of wedging it, and record the routing
                // decision so the request is still explainable.
                if matches!(circuit_decision, CircuitDecision::HalfOpen) {
                    release_half_open_probe(&state, candidate).await;
                }
                let result = format!("resolve error: {err:#}");
                trace_attempt(
                    &state,
                    &target,
                    client_id.as_deref(),
                    attempt,
                    candidate,
                    &result,
                    false,
                )
                .await;
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
        let upstream = format!(
            "{}{}",
            provider.base_url.trim_end_matches('/'),
            provider.kind.upstream_path()
        );

        let mut builder = state.client.post(upstream).body(body);
        builder = copy_request_headers(builder, &parts.headers, provider.kind);
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
                trace_attempt(
                    &state,
                    &target,
                    client_id.as_deref(),
                    attempt,
                    candidate,
                    &result,
                    can_fallback,
                )
                .await;
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
                trace_attempt(
                    &state,
                    &target,
                    client_id.as_deref(),
                    attempt,
                    candidate,
                    &result,
                    can_fallback,
                )
                .await;
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
            trace_attempt(
                &state,
                &target,
                client_id.as_deref(),
                attempt,
                candidate,
                &result,
                can_fallback,
            )
            .await;
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
            if matches!(provider.kind, ProviderKind::OpenAICompatible) {
                return translated_error_response(upstream_response).await;
            }
            return proxy_response(upstream_response);
        }

        circuit_success(&state, candidate, &route.policy.circuit_breaker).await;
        let result = format!("HTTP {status}");
        trace_attempt(
            &state,
            &target,
            client_id.as_deref(),
            attempt,
            candidate,
            &result,
            false,
        )
        .await;
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
        if matches!(provider.kind, ProviderKind::OpenAICompatible) {
            if !status.is_success() {
                // A non-fallback error status (400/401/500 ...) is terminal:
                // translate the OpenAI error body, preserve the status code.
                return translated_error_response(upstream_response).await;
            }
            if client_wants_stream && response_is_event_stream(&upstream_response) {
                return translated_sse_response(upstream_response, &model_id);
            }
            return translated_json_response(upstream_response, &model_id).await;
        }
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

// ===========================================================================
// openai-compatible translation wiring (v0.4 M2)
// ===========================================================================

/// Prepare the upstream request body for one candidate. Anthropic kinds keep
/// the byte-identical model rewrite; openai-compatible candidates get the full
/// Anthropic -> OpenAI translation. A translation failure is a resolve error
/// (pre-send), so it rides the existing resolve-error path.
fn prepare_request_body(bytes: &Bytes, model_id: &str, kind: ProviderKind) -> Result<Vec<u8>> {
    match kind {
        ProviderKind::Anthropic | ProviderKind::AnthropicCompatible => {
            rewrite_model(bytes.clone(), model_id)
        }
        ProviderKind::OpenAICompatible => {
            let body: Value =
                serde_json::from_slice(bytes).context("request body is not valid JSON")?;
            let prepared = translate::translate_request(&body, model_id)
                .context("failed to translate request")?;
            serde_json::to_vec(&prepared.body)
                .context("failed to serialize translated request body")
        }
    }
}

/// True when the upstream response declares an SSE body (media type
/// `text/event-stream`, parameters ignored). A gateway answering a whole
/// JSON body on a stream request must go through the buffered JSON
/// translation path — feeding it to the SSE state machine would find no
/// `data:` lines and synthesize an empty "clean" message over content the
/// proxy never parsed (the second-round review's whole-body variant).
fn response_is_event_stream(response: &reqwest::Response) -> bool {
    response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value
                .split(';')
                .next()
                .unwrap_or("")
                .trim()
                .eq_ignore_ascii_case("text/event-stream")
        })
}

/// Accepted streaming response from an openai-compatible upstream: translate
/// the OpenAI SSE chunk stream into an Anthropic event stream on the fly.
/// Headers are synthesized (content-type only); the upstream headers are not
/// forwarded. The response is already committed, so a translation failure
/// emits the terminal `error` event and ends the body — never a mid-stream
/// failover and never a faked clean close (invariant 5).
fn translated_sse_response(
    upstream_response: reqwest::Response,
    model_id: &str,
) -> Result<Response<Body>> {
    let status = upstream_response.status();
    let stream = TranslatedSseStream {
        upstream: Box::pin(upstream_response.bytes_stream()),
        translator: SseTranslator::new(model_id),
        pending: VecDeque::new(),
        finished: false,
    };
    Response::builder()
        .status(status.as_u16())
        .header(header::CONTENT_TYPE, "text/event-stream")
        .body(Body::from_stream(stream))
        .context("failed to build translated streaming response")
}

/// Accepted non-streaming response from an openai-compatible upstream: buffer
/// the body once and translate it to an Anthropic message JSON. The commit
/// point was the response headers, so a translation failure does not retry or
/// fall back; it surfaces a 502 Anthropic error envelope instead.
async fn translated_json_response(
    upstream_response: reqwest::Response,
    model_id: &str,
) -> Result<Response<Body>> {
    let status = upstream_response.status();
    let body = upstream_response
        .bytes()
        .await
        .context("failed to read upstream response body")?;
    let message = match serde_json::from_slice::<Value>(&body) {
        // A 200 body carrying an error (misbehaving gateways): translate the
        // error envelope and preserve the status instead of failing the
        // choices[0] extraction into a generic 502 that loses the upstream
        // type and message.
        Ok(openai) if translate::carries_error(&openai) => {
            return chunked_json_response(
                status,
                serde_json::to_vec(&translate::translate_error_body(&openai))
                    .context("failed to serialize error response")?,
            );
        }
        Ok(openai) => translate::translate_response(&openai, model_id)
            .map_err(|err| anyhow::anyhow!("failed to translate response: {err}")),
        Err(err) => Err(anyhow::anyhow!(
            "failed to translate response: upstream body is not valid JSON: {err}"
        )),
    };
    match message {
        Ok(message) => chunked_json_response(
            status,
            serde_json::to_vec(&message).context("failed to serialize translated response")?,
        ),
        Err(err) => chunked_json_response(
            StatusCode::BAD_GATEWAY,
            serde_json::to_vec(&translation_failure_body(&format!("{err:#}")))
                .context("failed to serialize error response")?,
        ),
    }
}

/// Terminal (non-fallback) error response from an openai-compatible upstream:
/// translate the OpenAI error body into the Anthropic error envelope. The
/// upstream status code is preserved.
async fn translated_error_response(upstream_response: reqwest::Response) -> Result<Response<Body>> {
    let status = upstream_response.status();
    let body = upstream_response
        .bytes()
        .await
        .context("failed to read upstream error body")?;
    let openai = serde_json::from_slice::<Value>(&body).unwrap_or_else(|_| {
        // A non-JSON error body keeps a fragment of the raw text.
        let fragment: String = String::from_utf8_lossy(&body).chars().take(120).collect();
        serde_json::json!({"error": {"message": fragment}})
    });
    let envelope = translate::translate_error_body(&openai);
    chunked_json_response(
        status,
        serde_json::to_vec(&envelope).context("failed to serialize error response")?,
    )
}

/// Anthropic error envelope for post-commit translation failures.
fn translation_failure_body(detail: &str) -> Value {
    serde_json::json!({
        "type": "error",
        "error": {
            "type": "api_error",
            "message": format!("ccm: upstream translation failure: {detail}"),
        }
    })
}

/// JSON response without content-length (chunked), used by the translated
/// non-streaming paths so the body framing matches `proxy_response`.
fn chunked_json_response(status: StatusCode, body: Vec<u8>) -> Result<Response<Body>> {
    Response::builder()
        .status(status.as_u16())
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from_stream(OnceStream::new(body)))
        .context("failed to build translated response")
}

/// Incremental translator from an OpenAI SSE byte stream to Anthropic SSE
/// event bytes. Every upstream chunk is fed through [`SseTranslator`] as it
/// arrives and every translated event is emitted immediately (invariant 6:
/// the whole stream is never buffered; only the current partial frame lives
/// in the translator).
struct TranslatedSseStream {
    upstream: Pin<Box<dyn Stream<Item = reqwest::Result<Bytes>> + Send>>,
    translator: SseTranslator,
    pending: VecDeque<Bytes>,
    finished: bool,
}

impl TranslatedSseStream {
    fn push_events(&mut self, events: Vec<SseEvent>) {
        for event in events {
            self.pending.push_back(Bytes::from(event.to_wire_bytes()));
        }
    }

    /// Terminal `error` event followed by body end.
    fn push_error(&mut self, detail: String) {
        let event = translate::error_event(&detail);
        self.pending.push_back(Bytes::from(event.to_wire_bytes()));
        self.finished = true;
    }
}

impl Stream for TranslatedSseStream {
    type Item = reqwest::Result<Bytes>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        loop {
            if let Some(bytes) = this.pending.pop_front() {
                return Poll::Ready(Some(Ok(bytes)));
            }
            if this.finished {
                return Poll::Ready(None);
            }
            match this.upstream.as_mut().poll_next(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Some(Ok(chunk))) => match this.translator.feed(&chunk) {
                    Ok(events) => {
                        this.push_events(events);
                        if this.translator.is_done() {
                            // The translator ended the event stream (a `[DONE]`
                            // close or a terminal error event). End the client
                            // body now instead of waiting for upstream EOF — a
                            // gateway holding the connection open must not
                            // hang an already-complete response.
                            this.finished = true;
                        }
                    }
                    Err(err) => this.push_error(err.to_string()),
                },
                // Mid-stream transport break: error event, never a faked clean close.
                Poll::Ready(Some(Err(err))) => {
                    this.push_error(format!("upstream stream broke mid-response: {err}"));
                }
                Poll::Ready(None) => {
                    this.finished = true;
                    match this.translator.finish() {
                        Ok(events) => this.push_events(events),
                        Err(err) => this.push_error(err.to_string()),
                    }
                }
            }
        }
    }
}

/// Single-chunk stream used to serve already-buffered JSON without a
/// content-length header (unknown size hint forces chunked framing).
struct OnceStream {
    body: Option<Bytes>,
}

impl OnceStream {
    fn new(body: Vec<u8>) -> Self {
        Self {
            body: Some(Bytes::from(body)),
        }
    }
}

impl Stream for OnceStream {
    type Item = Result<Bytes, Infallible>;

    fn poll_next(mut self: Pin<&mut Self>, _cx: &mut TaskContext<'_>) -> Poll<Option<Self::Item>> {
        Poll::Ready(self.body.take().map(Ok))
    }
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
    kind: ProviderKind,
) -> reqwest::RequestBuilder {
    // anthropic-version is meaningless to OpenAI-protocol upstreams.
    let strip_anthropic_version = matches!(kind, ProviderKind::OpenAICompatible);
    headers.iter().fold(builder, |builder, (name, value)| {
        // x-ccm-client is local identity and must never reach an upstream
        // provider, for any provider kind.
        if is_hop_by_hop(name.as_str())
            || name == header::HOST
            || name == header::CONTENT_LENGTH
            || name.as_str().eq_ignore_ascii_case("x-api-key")
            || name == header::AUTHORIZATION
            || name.as_str().eq_ignore_ascii_case(CLIENT_HEADER)
            || (strip_anthropic_version && name.as_str().eq_ignore_ascii_case("anthropic-version"))
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
    use crate::control::api::{
        apply_switch, control_clients, control_decisions, control_status, control_switch,
        control_traces, status_view, ClientParams,
    };
    use crate::route::SelectionWeights;
    use crate::routing::decision::now_ms;
    use crate::routing::metrics::{health_score, success_rate, update_latency_ewma};
    use crate::routing::select::{candidate_health_rank, weighted_score};
    use axum::extract::{Path, Query};
    use axum::{routing::post, Router};
    use serde_json::json;

    /// All four integration tests mutate process env (`CCM_HOME`,
    /// `CCM_<PROVIDER>_API_KEY`); each holds this lock for its whole duration
    /// so they never race (V0.3_PLAN §11.2 discipline).
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn env_guard() -> std::sync::MutexGuard<'static, ()> {
        ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

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
        // 200 + text/event-stream with scriptable raw SSE bytes (the
        // openai-compatible upstream contract).
        OpenaiSse(String),
        // Same, but the body never ends after the scripted bytes — a gateway
        // holding the connection open after its final frame.
        OpenaiSseHeldOpen(String),
        // 200 + application/json body (a complete OpenAI chat completion).
        OpenaiJson(String),
        // status + application/json body (OpenAI-shaped error envelope).
        OpenaiError(StatusCode, String),
    }

    #[derive(Clone)]
    struct CapturedRequest {
        path: String,
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
        uri: axum::http::Uri,
        headers: HeaderMap,
        body: Bytes,
    ) -> Response<Body> {
        let body: Value = serde_json::from_slice(&body).unwrap();
        state.requests.lock().unwrap().push(CapturedRequest {
            path: uri.path().to_string(),
            headers,
            body,
        });

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
            MockBehavior::OpenaiSse(raw) => Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, "text/event-stream")
                .body(Body::from(raw))
                .unwrap(),
            MockBehavior::OpenaiSseHeldOpen(raw) => Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, "text/event-stream")
                .body(Body::from_stream(HeldOpenStream {
                    body: Some(Bytes::from(raw)),
                }))
                .unwrap(),
            MockBehavior::OpenaiJson(body) => Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body))
                .unwrap(),
            MockBehavior::OpenaiError(status, body) => Response::builder()
                .status(status)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body))
                .unwrap(),
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
            .route("/v1/chat/completions", post(mock_messages))
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
            clients: Arc::new(RwLock::new(HashMap::new())),
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

    // The env mutex guard is held across awaits on purpose: it serializes
    // this whole test against the other env-mutating integration test. Each
    // test owns its current-thread runtime, so nothing on that runtime
    // contends for the lock.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn mock_provider_integration_covers_v03_routing_contract() {
        let _env_guard = env_guard();
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

    // ------------------------------------------------------------------
    // openai-compatible translation contract (v0.4 M2)
    // ------------------------------------------------------------------

    /// A well-formed OpenAI SSE chunk stream producing "Hello world" plus a
    /// final usage frame and [DONE].
    fn openai_stream_body() -> String {
        let f1 = json!({"id": "chatcmpl-1",
                        "choices": [{"index": 0, "delta": {"role": "assistant", "content": "Hello"}}]});
        let f2 = json!({"id": "chatcmpl-1",
                        "choices": [{"index": 0, "delta": {"content": " world"}, "finish_reason": null}]});
        let f3 = json!({"id": "chatcmpl-1",
                        "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]});
        let f4 = json!({"id": "chatcmpl-1", "choices": [],
                        "usage": {"prompt_tokens": 10, "completion_tokens": 2}});
        format!("data: {f1}\n\ndata: {f2}\n\ndata: {f3}\n\ndata: {f4}\n\ndata: [DONE]\n\n")
    }

    /// Rich Anthropic request exercising the translation surface: system,
    /// tools, tool_use/tool_result, cache_control, thinking, stream.
    fn openai_integration_request() -> Request<Body> {
        Request::builder()
            .method(Method::POST)
            .uri("/v1/messages")
            .header(header::CONTENT_TYPE, "application/json")
            .header("anthropic-version", "2023-06-01")
            .body(Body::from(
                r#"{
                    "model": "ccm",
                    "max_tokens": 64,
                    "stream": true,
                    "system": "be brief",
                    "thinking": {"type": "enabled", "budget_tokens": 1024},
                    "tools": [{
                        "name": "get_weather",
                        "description": "weather lookup",
                        "input_schema": {
                            "type": "object",
                            "properties": {"city": {"type": "string"}}
                        }
                    }],
                    "messages": [
                        {"role": "user", "content": [
                            {"type": "text", "text": "weather?", "cache_control": {"type": "ephemeral"}}
                        ]},
                        {"role": "assistant", "content": [
                            {"type": "tool_use", "id": "toolu_1", "name": "get_weather", "input": {"city": "SF"}}
                        ]},
                        {"role": "user", "content": [
                            {"type": "tool_result", "tool_use_id": "toolu_1", "content": "sunny"}
                        ]}
                    ]
                }"#,
            ))
            .unwrap()
    }

    /// Non-streaming counterpart of `openai_integration_request`: same
    /// surface, `stream: false`.
    fn openai_non_stream_request() -> Request<Body> {
        Request::builder()
            .method(Method::POST)
            .uri("/v1/messages")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                r#"{"model":"ccm","max_tokens":64,"stream":false,"messages":[{"role":"user","content":"hi"}]}"#,
            ))
            .unwrap()
    }

    fn write_openai_integration_config(root: &std::path::Path, openai_url: &str, native_url: &str) {
        std::fs::create_dir_all(root).unwrap();
        let raw = format!(
            r#"
[providers.openai]
kind = "openai-compatible"
base_url = "{openai_url}"

[providers.native]
kind = "anthropic-compatible"
base_url = "{native_url}"
auth = "x-api-key"

[models.openai]
provider = "openai"
model_id = "deepseek-chat"

[models.native]
provider = "native"
model_id = "upstream-native"

[routes.test-route]
primary = "openai"
fallback = ["native"]

[routes.test-route.policy]
selection = "ordered"
header_timeout_ms = 250
fallback_on = [429, 503]
max_attempts = 2
backoff_ms = 0

[routes.test-route.policy.circuit_breaker]
enabled = true
failure_threshold = 1
open_ms = 30
"#
        );
        std::fs::write(root.join("config.toml"), raw).unwrap();
    }

    /// Test-only stream that yields the scripted bytes once, then stays open
    /// forever (never EOF) — an upstream gateway holding the connection after
    /// its final SSE frame.
    struct HeldOpenStream {
        body: Option<Bytes>,
    }

    impl Stream for HeldOpenStream {
        type Item = Result<Bytes, Infallible>;

        fn poll_next(
            mut self: Pin<&mut Self>,
            _cx: &mut TaskContext<'_>,
        ) -> Poll<Option<Self::Item>> {
            if let Some(bytes) = self.body.take() {
                return Poll::Ready(Some(Ok(bytes)));
            }
            Poll::Pending
        }
    }

    /// Parse client-visible SSE text into (event name, data JSON) pairs.
    fn parse_client_sse(text: &str) -> Vec<(String, Value)> {
        text.split("\n\n")
            .filter(|frame| !frame.trim().is_empty())
            .map(|frame| {
                let mut event = String::new();
                let mut data = String::new();
                for line in frame.lines() {
                    if let Some(rest) = line.strip_prefix("event: ") {
                        event = rest.to_string();
                    } else if let Some(rest) = line.strip_prefix("data: ") {
                        data = rest.to_string();
                    }
                }
                (event, serde_json::from_str(&data).unwrap())
            })
            .collect()
    }

    #[allow(clippy::await_holding_lock)] // see note on the v0.3 test above
    #[tokio::test]
    async fn mock_openai_provider_integration_covers_translation_contract() {
        let _env_guard = env_guard();
        let root = std::env::temp_dir().join(format!(
            "ccm-openai-integration-{}-{}",
            std::process::id(),
            now_ms()
        ));
        std::env::set_var("CCM_HOME", &root);
        std::env::set_var("CCM_OPENAI_API_KEY", "openai-secret");
        std::env::set_var("CCM_NATIVE_API_KEY", "native-secret");

        let (openai_addr, openai, openai_task) =
            spawn_mock(MockBehavior::OpenaiSse(openai_stream_body())).await;
        let (native_addr, native, native_task) = spawn_mock(MockBehavior::OkStream).await;
        write_openai_integration_config(
            &root,
            &format!("http://{openai_addr}"),
            &format!("http://{native_addr}"),
        );
        let state = integration_proxy_state();

        // Streaming translation end to end: translated upstream body, Bearer
        // auth, header stripping, and a full Anthropic event sequence back.
        let response = forward(state.clone(), openai_integration_request())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get(header::CONTENT_TYPE)
                .unwrap()
                .to_str()
                .unwrap(),
            "text/event-stream"
        );
        assert!(response.headers().get(header::CONTENT_LENGTH).is_none());
        let events = parse_client_sse(&body_text(response).await);
        let names: Vec<&str> = events.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(
            names,
            [
                "message_start",
                "content_block_start",
                "content_block_delta",
                "content_block_delta",
                "content_block_stop",
                "message_delta",
                "message_stop",
            ]
        );
        let text: String = events
            .iter()
            .filter(|(name, _)| name == "content_block_delta")
            .filter_map(|(_, data)| data["delta"]["text"].as_str().map(str::to_string))
            .collect();
        assert_eq!(text, "Hello world");
        {
            let requests = openai.requests.lock().unwrap();
            assert_eq!(requests.len(), 1);
            let captured = requests.last().unwrap();
            assert_eq!(captured.path, "/v1/chat/completions");
            assert_eq!(
                captured
                    .headers
                    .get(header::AUTHORIZATION)
                    .unwrap()
                    .to_str()
                    .unwrap(),
                "Bearer openai-secret"
            );
            assert!(captured.headers.get("x-api-key").is_none());
            assert!(captured.headers.get("anthropic-version").is_none());

            assert_eq!(captured.body["model"], "deepseek-chat");
            let messages = captured.body["messages"].as_array().unwrap();
            assert_eq!(
                messages[0],
                json!({"role": "system", "content": "be brief"})
            );
            assert_eq!(messages[1], json!({"role": "user", "content": "weather?"}));
            assert_eq!(messages[2]["role"], "assistant");
            assert_eq!(
                messages[2]["tool_calls"][0]["function"]["name"],
                "get_weather"
            );
            assert_eq!(
                messages[3],
                json!({"role": "tool", "tool_call_id": "toolu_1", "content": "sunny"})
            );
            assert_eq!(captured.body["tools"][0]["type"], "function");
            assert_eq!(captured.body["tools"][0]["function"]["name"], "get_weather");
            assert_eq!(
                captured.body["tools"][0]["function"]["parameters"]["properties"]["city"],
                json!({"type": "string"})
            );
            assert_eq!(captured.body["stream"], json!(true));
            assert_eq!(
                captured.body["stream_options"],
                json!({"include_usage": true})
            );
            let serialized = captured.body.to_string();
            assert!(!serialized.contains("cache_control"));
            assert!(!serialized.contains("thinking"));
        }

        // Terminal (non-fallback) upstream error: status preserved, body
        // translated to the Anthropic error envelope.
        *openai.behavior.write().await = MockBehavior::OpenaiError(
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({"error": {"message": "upstream boom", "type": "server_error"}}).to_string(),
        );
        let response = forward(state.clone(), openai_integration_request())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let error_body: Value = serde_json::from_str(&body_text(response).await).unwrap();
        assert_eq!(
            error_body,
            json!({"type": "error", "error": {"type": "api_error", "message": "upstream boom"}})
        );

        // Non-streaming translation: the client's `stream: false` yields a
        // buffered Anthropic message JSON, chunked (no content-length).
        *openai.behavior.write().await = MockBehavior::OpenaiJson(
            json!({
                "id": "chatcmpl-ns",
                "object": "chat.completion",
                "choices": [{
                    "index": 0,
                    "message": {"role": "assistant", "content": "Hello world"},
                    "finish_reason": "stop"
                }],
                "usage": {"prompt_tokens": 12, "completion_tokens": 2}
            })
            .to_string(),
        );
        let openai_before = openai.requests.lock().unwrap().len();
        let response = forward(state.clone(), openai_non_stream_request())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get(header::CONTENT_TYPE)
                .unwrap()
                .to_str()
                .unwrap(),
            "application/json"
        );
        assert!(response.headers().get(header::CONTENT_LENGTH).is_none());
        let message: Value = serde_json::from_str(&body_text(response).await).unwrap();
        assert_eq!(message["type"], "message");
        assert_eq!(message["role"], "assistant");
        assert_eq!(message["model"], "deepseek-chat");
        assert_eq!(
            message["content"][0],
            json!({"type": "text", "text": "Hello world"})
        );
        assert_eq!(message["stop_reason"], "end_turn");
        assert_eq!(message["usage"]["input_tokens"], 12);
        assert_eq!(message["usage"]["output_tokens"], 2);
        {
            let requests = openai.requests.lock().unwrap();
            assert_eq!(requests.len(), openai_before + 1);
            let captured = requests.last().unwrap();
            assert_eq!(captured.path, "/v1/chat/completions");
            assert_eq!(captured.body["stream"], json!(false));
        }

        // Post-commit translation failure (plan §8.2): an untranslatable 200
        // body surfaces a 502 Anthropic envelope with NO retry and NO
        // fallback — exactly one upstream request, zero fallback requests.
        *openai.behavior.write().await =
            MockBehavior::OpenaiJson(json!({"unexpected": true}).to_string());
        let openai_before = openai.requests.lock().unwrap().len();
        let native_before = native.requests.lock().unwrap().len();
        let response = forward(state.clone(), openai_non_stream_request())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        let failure: Value = serde_json::from_str(&body_text(response).await).unwrap();
        assert_eq!(failure["type"], "error");
        assert_eq!(failure["error"]["type"], "api_error");
        assert!(failure["error"]["message"]
            .as_str()
            .unwrap()
            .starts_with("ccm: upstream translation failure:"));
        assert_eq!(openai.requests.lock().unwrap().len(), openai_before + 1);
        assert_eq!(native.requests.lock().unwrap().len(), native_before);

        // Mixed-protocol fallback: openai primary returns 429, native
        // anthropic fallback serves the response as native Anthropic SSE.
        *openai.behavior.write().await = MockBehavior::Status(StatusCode::TOO_MANY_REQUESTS);
        let native_before = native.requests.lock().unwrap().len();
        let response = forward(state.clone(), openai_integration_request())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert!(body_text(response).await.contains("message_start"));
        {
            let requests = native.requests.lock().unwrap();
            assert_eq!(requests.len(), native_before + 1);
            let captured = requests.last().unwrap();
            assert_eq!(captured.path, "/v1/messages");
            assert_eq!(captured.body["model"], "upstream-native");
            assert_eq!(
                captured.headers.get("x-api-key").unwrap().to_str().unwrap(),
                "native-secret"
            );
        }
        {
            let decisions = state.decisions.read().await;
            let decision = decisions.back().unwrap();
            assert_eq!(decision.selected.as_deref(), Some("native"));
            assert_eq!(decision.attempts.len(), 2);
            assert!(decision.attempts[0].result.contains("429"));
            assert!(decision.attempts[0].fallback);
        }

        // Committed-stream failure: a bad SSE frame after 200 headers yields
        // the terminal error event, never a mid-stream failover.
        sleep(Duration::from_millis(40)).await; // let the 429-opened circuit cooldown elapse
        let partial = format!(
            "data: {}\n\n",
            json!({"id": "chatcmpl-bad",
                   "choices": [{"index": 0, "delta": {"content": "partial"}}]})
        );
        *openai.behavior.write().await =
            MockBehavior::OpenaiSse(format!("{partial}data: {{not json}}\n\n"));
        let native_before = native.requests.lock().unwrap().len();
        let response = forward(state.clone(), openai_integration_request())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let client_sse = body_text(response).await;
        assert!(client_sse.contains("event: error"));
        assert!(!client_sse.contains("message_stop"));
        // No mid-stream failover: the anthropic fallback mock received zero
        // requests after the response was committed.
        assert_eq!(native.requests.lock().unwrap().len(), native_before);

        // Terminal-event termination: a gateway that HOLDS the 200 connection
        // open after its final frame must not hang the client response — the
        // body ends as soon as the translator reaches its terminal state.
        let held_partial = format!(
            "data: {}\n\n",
            json!({"id": "chatcmpl-held",
                   "choices": [{"index": 0, "delta": {"content": "partial"}}]})
        );
        let held_error = json!({"error": {"message": "held open", "type": "server_error"}});
        *openai.behavior.write().await =
            MockBehavior::OpenaiSseHeldOpen(format!("{held_partial}data: {held_error}\n\n"));
        let response = forward(state.clone(), openai_integration_request())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let held_sse = tokio::time::timeout(Duration::from_secs(5), body_text(response))
            .await
            .expect("client body completes without upstream EOF");
        assert!(held_sse.contains("event: error"));
        assert!(!held_sse.contains("message_stop"));

        // Non-SSE body on a stream request: a gateway that ignores `stream`
        // and answers a whole JSON completion goes through the buffered JSON
        // translation path instead of the SSE state machine (which would
        // synthesize an empty message over content it never parsed).
        *openai.behavior.write().await = MockBehavior::OpenaiJson(
            json!({
                "id": "chatcmpl-ns2",
                "object": "chat.completion",
                "choices": [{
                    "index": 0,
                    "message": {"role": "assistant", "content": "whole body reply"},
                    "finish_reason": "stop"
                }],
                "usage": {"prompt_tokens": 5, "completion_tokens": 3}
            })
            .to_string(),
        );
        let response = forward(state.clone(), openai_integration_request())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get(header::CONTENT_TYPE)
                .unwrap()
                .to_str()
                .unwrap(),
            "application/json"
        );
        let message: Value = serde_json::from_str(&body_text(response).await).unwrap();
        assert_eq!(message["type"], "message");
        assert_eq!(message["content"][0]["text"], "whole body reply");

        // A 200 body carrying an error (misbehaving gateways): status kept,
        // upstream type and message preserved through the envelope
        // translation instead of a generic 502.
        *openai.behavior.write().await = MockBehavior::OpenaiJson(
            json!({"error": {"message": "late failure", "type": "rate_limit_error"}}).to_string(),
        );
        let response = forward(state.clone(), openai_non_stream_request())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let error_body: Value = serde_json::from_str(&body_text(response).await).unwrap();
        assert_eq!(
            error_body,
            json!({"type": "error", "error": {"type": "rate_limit_error", "message": "late failure"}})
        );

        // HALF_OPEN probe release on translate failure: a request the
        // translator rejects must release an in-flight probe and record the
        // resolve error instead of wedging the model.
        *openai.behavior.write().await = MockBehavior::Status(StatusCode::SERVICE_UNAVAILABLE);
        let response = forward(state.clone(), openai_integration_request())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK); // native fallback answered
        sleep(Duration::from_millis(40)).await; // cooldown elapsed, next probe is HALF_OPEN
        *openai.behavior.write().await = MockBehavior::OpenaiSse(openai_stream_body());
        let rejected = Request::builder()
            .method(Method::POST)
            .uri("/v1/messages")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                r#"{"model":"ccm","max_tokens":16,"messages":"not an array"}"#,
            ))
            .unwrap();
        assert!(forward(state.clone(), rejected).await.is_err());
        {
            let decisions = state.decisions.read().await;
            let decision = decisions.back().unwrap();
            assert!(decision.outcome.contains("resolve error"));
            assert!(decision.outcome.contains("translate"));
            assert_eq!(decision.attempts[0].circuit, "HALF_OPEN");
            assert!(decision.attempts[0].result.contains("translate"));
        }

        // The released probe slot admits the next request as HALF_OPEN again,
        // and this time the translated stream succeeds.
        let response = forward(state.clone(), openai_integration_request())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert!(body_text(response).await.contains("message_stop"));
        {
            let decisions = state.decisions.read().await;
            let decision = decisions.back().unwrap();
            assert_eq!(decision.attempts[0].circuit, "HALF_OPEN");
            assert_eq!(decision.selected.as_deref(), Some("openai"));
        }

        // Health check follows the kind: the openai model pings
        // /v1/chat/completions with Bearer auth, max_tokens 1, and no
        // anthropic-version header.
        let health_before = openai.requests.lock().unwrap().len();
        crate::health::check(&AppConfig::load().unwrap(), "openai")
            .await
            .unwrap();
        {
            let requests = openai.requests.lock().unwrap();
            assert_eq!(requests.len(), health_before + 1);
            let captured = requests.last().unwrap();
            assert_eq!(captured.path, "/v1/chat/completions");
            assert_eq!(
                captured
                    .headers
                    .get(header::AUTHORIZATION)
                    .unwrap()
                    .to_str()
                    .unwrap(),
                "Bearer openai-secret"
            );
            assert!(captured.headers.get("anthropic-version").is_none());
            assert_eq!(captured.body["model"], "deepseek-chat");
            assert_eq!(captured.body["max_tokens"], 1);
            assert_eq!(captured.body["messages"][0]["content"], "ping");
        }

        openai_task.abort();
        native_task.abort();
        std::env::remove_var("CCM_HOME");
        std::env::remove_var("CCM_OPENAI_API_KEY");
        std::env::remove_var("CCM_NATIVE_API_KEY");
        let _ = std::fs::remove_dir_all(root);
    }

    // ------------------------------------------------------------------
    // multi-client scoped switching contract (v0.4 M3)
    // ------------------------------------------------------------------

    fn write_multi_client_config(root: &std::path::Path, x_url: &str, y_url: &str) {
        std::fs::create_dir_all(root).unwrap();
        let raw = format!(
            r#"
[providers.px]
kind = "anthropic-compatible"
base_url = "{x_url}"
auth = "x-api-key"

[providers.py]
kind = "anthropic-compatible"
base_url = "{y_url}"
auth = "bearer"

[models.modelx]
provider = "px"
model_id = "upstream-x"

[models.modely]
provider = "py"
model_id = "upstream-y"
"#
        );
        std::fs::write(root.join("config.toml"), raw).unwrap();
    }

    /// A `/v1/messages` request with optional identity channels: the
    /// `x-ccm-client` header and/or an `Authorization` bearer token.
    fn client_tagged_request(
        client_header: Option<&str>,
        authorization: Option<&str>,
    ) -> Request<Body> {
        let mut builder = Request::builder()
            .method(Method::POST)
            .uri("/v1/messages")
            .header(header::CONTENT_TYPE, "application/json");
        if let Some(id) = client_header {
            builder = builder.header("x-ccm-client", id);
        }
        if let Some(token) = authorization {
            builder = builder.header(header::AUTHORIZATION, token);
        }
        builder
            .body(Body::from(
                r#"{"model":"ccm","max_tokens":16,"messages":[{"role":"user","content":"ping"}]}"#,
            ))
            .unwrap()
    }

    #[allow(clippy::await_holding_lock)] // see note on the v0.3 test above
    #[tokio::test]
    async fn multi_client_integration_covers_scoped_switching_contract() {
        let _env_guard = env_guard();
        let root = std::env::temp_dir().join(format!(
            "ccm-client-integration-{}-{}",
            std::process::id(),
            now_ms()
        ));
        std::env::set_var("CCM_HOME", &root);
        std::env::set_var("CCM_PX_API_KEY", "x-secret");
        std::env::set_var("CCM_PY_API_KEY", "y-secret");

        let (x_addr, x_mock, x_task) = spawn_mock(MockBehavior::OkStream).await;
        let (y_addr, y_mock, y_task) = spawn_mock(MockBehavior::OkStream).await;
        write_multi_client_config(
            &root,
            &format!("http://{x_addr}"),
            &format!("http://{y_addr}"),
        );
        let state = integration_proxy_state();
        // Global runtime target: modelx (mock X). Bare model targets resolve
        // with the default policy (ordered, single candidate, always-200
        // mocks => deterministic, no backoff ever fires).
        *state.target.write().await = "modelx".to_string();

        // Scoped switches: A -> modely, B -> modelx. Entries exist ONLY from
        // here. A scoped switch never moves the global target.
        apply_switch(&state, "modely", Some("A")).await.unwrap();
        apply_switch(&state, "modelx", Some("B")).await.unwrap();
        assert_eq!(*state.target.read().await, "modelx");

        let x_before = x_mock.requests.lock().unwrap().len();
        let y_before = y_mock.requests.lock().unwrap().len();

        // A routes to ITS entry (modely -> mock Y); simultaneously, on the
        // same proxy state, B routes to its entry (modelx -> mock X). The
        // upstream body carries the per-target model rewrite, proving which
        // mock served each client. Strip proof: no identity headers upstream.
        let response = forward(state.clone(), client_tagged_request(Some("A"), None))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert!(body_text(response).await.contains("message_start"));
        assert_eq!(y_mock.requests.lock().unwrap().len(), y_before + 1);
        assert_eq!(x_mock.requests.lock().unwrap().len(), x_before);
        {
            let requests = y_mock.requests.lock().unwrap();
            let captured = requests.last().unwrap();
            assert_eq!(captured.body["model"], "upstream-y");
            assert!(captured.headers.get("x-ccm-client").is_none());
            // py injects its bearer credential; no client identity rides along.
            assert_eq!(
                captured
                    .headers
                    .get(header::AUTHORIZATION)
                    .unwrap()
                    .to_str()
                    .unwrap(),
                "Bearer y-secret"
            );
        }

        let response = forward(state.clone(), client_tagged_request(Some("B"), None))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(x_mock.requests.lock().unwrap().len(), x_before + 1);
        {
            let requests = x_mock.requests.lock().unwrap();
            let captured = requests.last().unwrap();
            assert_eq!(captured.body["model"], "upstream-x");
            assert_eq!(
                captured.headers.get("x-api-key").unwrap().to_str().unwrap(),
                "x-secret"
            );
            assert!(captured.headers.get("x-ccm-client").is_none());
            assert!(captured.headers.get(header::AUTHORIZATION).is_none());
        }
        {
            let clients = state.clients.read().await;
            assert_eq!(clients.len(), 2);
            assert_eq!(clients.get("A").unwrap().target, "modely");
            assert_eq!(clients.get("A").unwrap().requests, 1);
            assert!(clients.get("A").unwrap().last_seen_ms > 0);
            assert_eq!(clients.get("B").unwrap().target, "modelx");
            assert_eq!(clients.get("B").unwrap().requests, 1);
        }

        // A no-id request follows the GLOBAL target and changes no entry:
        // nothing is created, no target moves, no counter bumps.
        let response = forward(state.clone(), client_tagged_request(None, None))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(x_mock.requests.lock().unwrap().len(), x_before + 2);
        {
            let clients = state.clients.read().await;
            assert_eq!(clients.len(), 2);
            assert_eq!(clients.get("A").unwrap().requests, 1);
            assert_eq!(clients.get("B").unwrap().requests, 1);
            assert_eq!(clients.get("A").unwrap().target, "modely");
            assert_eq!(clients.get("B").unwrap().target, "modelx");
        }
        {
            let decisions = state.decisions.read().await;
            let decision = decisions.back().unwrap();
            assert_eq!(decision.client, None);
            assert_eq!(decision.target, "modelx");
        }

        // A GLOBAL switch (no client param) moves only the global target;
        // scoped entries survive untouched.
        apply_switch(&state, "modely", None).await.unwrap();
        assert_eq!(*state.target.read().await, "modely");
        {
            let clients = state.clients.read().await;
            assert_eq!(clients.get("B").unwrap().target, "modelx");
        }
        // B still routes per its own entry (mock X), not the moved global
        // target — a scoped switch does not affect the other client.
        let response = forward(state.clone(), client_tagged_request(Some("B"), None))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(x_mock.requests.lock().unwrap().len(), x_before + 3);
        // ... while a no-id request now follows the moved global target.
        let response = forward(state.clone(), client_tagged_request(None, None))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(y_mock.requests.lock().unwrap().len(), y_before + 2);
        {
            let clients = state.clients.read().await;
            assert_eq!(clients.get("B").unwrap().requests, 2);
        }

        // An unknown id (never switched) follows the global target and
        // creates NO entry.
        let response = forward(state.clone(), client_tagged_request(Some("ghost"), None))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(y_mock.requests.lock().unwrap().len(), y_before + 3);
        {
            let clients = state.clients.read().await;
            assert!(clients.get("ghost").is_none());
            assert_eq!(clients.len(), 2);
        }
        {
            let decisions = state.decisions.read().await;
            let decision = decisions.back().unwrap();
            assert_eq!(decision.client.as_deref(), Some("ghost"));
            assert_eq!(decision.target, "modely"); // effective == global
        }

        // Token channel: `Bearer ccm-local-foo` resolves client foo (no
        // entry => global). The placeholder token itself must not leak
        // upstream (Authorization is stripped).
        let response = forward(
            state.clone(),
            client_tagged_request(None, Some("Bearer ccm-local-foo")),
        )
        .await
        .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(y_mock.requests.lock().unwrap().len(), y_before + 4);
        {
            let requests = y_mock.requests.lock().unwrap();
            let captured = requests.last().unwrap();
            // The placeholder token was stripped and replaced by the upstream
            // credential — `ccm-local-foo` never reaches the provider.
            assert_eq!(
                captured
                    .headers
                    .get(header::AUTHORIZATION)
                    .unwrap()
                    .to_str()
                    .unwrap(),
                "Bearer y-secret"
            );
            assert!(captured.headers.get("x-ccm-client").is_none());
        }
        {
            let decisions = state.decisions.read().await;
            assert_eq!(decisions.back().unwrap().client.as_deref(), Some("foo"));
        }
        assert!(state.clients.read().await.get("foo").is_none());

        // Legacy v0.3 placeholder (`ccm-local` without an id) => no id.
        let response = forward(
            state.clone(),
            client_tagged_request(None, Some("Bearer ccm-local")),
        )
        .await
        .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(y_mock.requests.lock().unwrap().len(), y_before + 5);
        {
            let decisions = state.decisions.read().await;
            assert_eq!(decisions.back().unwrap().client, None);
        }

        // A real-looking bearer credential gets no id, is not parsed beyond
        // the prefix check, and never appears in any record or header.
        let response = forward(
            state.clone(),
            client_tagged_request(None, Some("Bearer sk-ant-supersecret")),
        )
        .await
        .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(y_mock.requests.lock().unwrap().len(), y_before + 6);
        {
            let requests = y_mock.requests.lock().unwrap();
            let captured = requests.last().unwrap();
            // The real credential was stripped; only the upstream credential
            // was injected. Nothing else about the token is recorded.
            assert_eq!(
                captured
                    .headers
                    .get(header::AUTHORIZATION)
                    .unwrap()
                    .to_str()
                    .unwrap(),
                "Bearer y-secret"
            );
        }
        {
            let decisions = state.decisions.read().await;
            assert_eq!(decisions.back().unwrap().client, None);
        }

        // Header precedence: with both channels present, x-ccm-client wins
        // over the token (B's entry -> mock X, not A's -> mock Y).
        let response = forward(
            state.clone(),
            client_tagged_request(Some("B"), Some("Bearer ccm-local-A")),
        )
        .await
        .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(x_mock.requests.lock().unwrap().len(), x_before + 4);
        assert_eq!(y_mock.requests.lock().unwrap().len(), y_before + 6);
        {
            let decisions = state.decisions.read().await;
            assert_eq!(decisions.back().unwrap().client.as_deref(), Some("B"));
        }

        // An out-of-charset request id is treated as NO id (global routing),
        // never rejected.
        let response = forward(state.clone(), client_tagged_request(Some("bad id!"), None))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(y_mock.requests.lock().unwrap().len(), y_before + 7);
        {
            let decisions = state.decisions.read().await;
            assert_eq!(decisions.back().unwrap().client, None);
        }

        // Scoped switch validation: invalid charset (and over-length) ids
        // fail — surfaced as HTTP 400 by the handler — and unknown targets
        // fail for BOTH scoped and global switches, changing nothing.
        let response = control_switch(
            State(state.clone()),
            Path("modelx".to_string()),
            Query(ClientParams {
                client: Some("bad id!".to_string()),
            }),
        )
        .await
        .into_response();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert!(body_text(response).await.contains("invalid client id"));
        assert!(apply_switch(&state, "modelx", Some(&"x".repeat(65)))
            .await
            .is_err());
        let response = control_switch(
            State(state.clone()),
            Path("no-such-target".to_string()),
            Query(ClientParams {
                client: Some("A".to_string()),
            }),
        )
        .await
        .into_response();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert!(apply_switch(&state, "no-such-target", None).await.is_err());
        {
            let clients = state.clients.read().await;
            assert_eq!(clients.len(), 2); // no failed-switch side effects
            assert_eq!(clients.get("A").unwrap().target, "modely");
            assert_eq!(clients.get("B").unwrap().target, "modelx");
        }

        // /_ccm/clients lists exactly the switched entries.
        let clients = control_clients(State(state.clone())).await.0;
        assert_eq!(clients.len(), 2);
        assert_eq!(clients[0].client, "A");
        assert_eq!(clients[0].target, "modely");
        assert_eq!(clients[0].requests, 1);
        assert!(clients[0].last_seen_ms > 0);
        assert_eq!(clients[1].client, "B");
        assert_eq!(clients[1].target, "modelx");
        assert_eq!(clients[1].requests, 3);

        // Re-switch (existing-entry branch): switching A AGAIN moves its
        // target while the arrival counter survives. Driven through the
        // control_switch handler to also pin the scoped SUCCESS body.
        let response = control_switch(
            State(state.clone()),
            Path("modelx".to_string()),
            Query(ClientParams {
                client: Some("A".to_string()),
            }),
        )
        .await
        .into_response();
        assert_eq!(response.status(), StatusCode::OK);
        let view: Value = serde_json::from_str(&body_text(response).await).unwrap();
        assert_eq!(view["client"], "A");
        assert_eq!(view["target"], "modelx");
        {
            let clients = state.clients.read().await;
            assert_eq!(clients.len(), 2); // updated in place, no new entry
            assert_eq!(clients.get("A").unwrap().target, "modelx"); // moved...
            assert_eq!(clients.get("A").unwrap().requests, 1); // ...counter kept
        }
        apply_switch(&state, "modely", Some("A")).await.unwrap(); // restore

        // ?client= filters on traces and decisions; records carry the field.
        let traces_a = control_traces(
            State(state.clone()),
            Query(ClientParams {
                client: Some("A".to_string()),
            }),
        )
        .await
        .0;
        assert!(!traces_a.is_empty());
        assert!(traces_a.iter().all(|t| t.client.as_deref() == Some("A")));
        let traces_all = control_traces(State(state.clone()), Query(ClientParams { client: None }))
            .await
            .0;
        assert!(traces_all.len() > traces_a.len());
        assert!(traces_all.iter().any(|t| t.client.is_none()));

        let decisions_b = control_decisions(
            State(state.clone()),
            Query(ClientParams {
                client: Some("B".to_string()),
            }),
        )
        .await
        .0;
        assert_eq!(decisions_b.len(), 3);
        assert!(decisions_b.iter().all(|d| d.client.as_deref() == Some("B")));
        let decisions_all =
            control_decisions(State(state.clone()), Query(ClientParams { client: None }))
                .await
                .0;
        assert_eq!(decisions_all.len(), 11);
        // Serialized records carry `client` only when present, and never the
        // real bearer credential.
        let serialized = serde_json::to_string(&decisions_all).unwrap();
        assert!(serialized.contains("\"client\":\"B\""));
        assert!(!serialized.contains("sk-ant-supersecret"));
        let traces_serialized = serde_json::to_string(&traces_all).unwrap();
        assert!(!traces_serialized.contains("sk-ant-supersecret"));
        // M5 freezes this JSONL schema: a record WITHOUT a client must not
        // grow a client:null key — skip_serializing_if keeps the no-id shape
        // byte-identical to v0.3. Pin one decision (its nested attempts
        // included) and one trace.
        let no_id_decision = decisions_all.iter().find(|d| d.client.is_none()).unwrap();
        assert!(!serde_json::to_string(no_id_decision)
            .unwrap()
            .contains("\"client\""));
        let no_id_trace = traces_all.iter().find(|t| t.client.is_none()).unwrap();
        assert!(!serde_json::to_string(no_id_trace)
            .unwrap()
            .contains("\"client\""));

        // Status with ?client= shows the client's EFFECTIVE view; without the
        // parameter the shape is unchanged (no client/follows_global keys).
        let response = control_status(
            State(state.clone()),
            Query(ClientParams {
                client: Some("B".to_string()),
            }),
        )
        .await
        .into_response();
        assert_eq!(response.status(), StatusCode::OK);
        let view: Value = serde_json::from_str(&body_text(response).await).unwrap();
        assert_eq!(view["target"], "modelx"); // B's entry
        assert_eq!(view["model_id"], "upstream-x");
        assert_eq!(view["client"], "B");
        assert_eq!(view["follows_global"], false);
        let response = control_status(
            State(state.clone()),
            Query(ClientParams {
                client: Some("ghost".to_string()),
            }),
        )
        .await
        .into_response();
        let view: Value = serde_json::from_str(&body_text(response).await).unwrap();
        assert_eq!(view["target"], "modely"); // falls back to global
        assert_eq!(view["client"], "ghost");
        assert_eq!(view["follows_global"], true);
        let response = control_status(State(state.clone()), Query(ClientParams { client: None }))
            .await
            .into_response();
        let view: Value = serde_json::from_str(&body_text(response).await).unwrap();
        assert_eq!(view["target"], "modely"); // the global view
        assert!(view.get("client").is_none());
        assert!(view.get("follows_global").is_none());

        x_task.abort();
        y_task.abort();
        std::env::remove_var("CCM_HOME");
        std::env::remove_var("CCM_PX_API_KEY");
        std::env::remove_var("CCM_PY_API_KEY");
        let _ = std::fs::remove_dir_all(root);
    }

    // ------------------------------------------------------------------
    // control-plane wire contract (v0.4 M3): real router, real HTTP,
    // real query strings
    // ------------------------------------------------------------------

    /// Serve the actual control router on an ephemeral loopback port and
    /// drive it with a real HTTP client — closing the gap between the
    /// handler-level tests above and route registration / query extraction,
    /// including the no-query-string shapes the v0.3 CLI sends.
    #[allow(clippy::await_holding_lock)] // see note on the v0.3 test above
    #[tokio::test]
    async fn control_router_serves_client_contract_over_http() {
        let _env_guard = env_guard();
        let root = std::env::temp_dir().join(format!(
            "ccm-control-wire-{}-{}",
            std::process::id(),
            now_ms()
        ));
        std::env::set_var("CCM_HOME", &root);
        std::env::set_var("CCM_PX_API_KEY", "x-secret");
        std::env::set_var("CCM_PY_API_KEY", "y-secret");

        let (x_addr, x_mock, x_task) = spawn_mock(MockBehavior::OkStream).await;
        let (y_addr, y_mock, y_task) = spawn_mock(MockBehavior::OkStream).await;
        write_multi_client_config(
            &root,
            &format!("http://{x_addr}"),
            &format!("http://{y_addr}"),
        );
        let state = integration_proxy_state();
        *state.target.write().await = "modelx".to_string();

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let router_task = tokio::spawn(async move {
            axum::serve(listener, control_router(state)).await.unwrap();
        });
        let http = Client::new();

        // The v0.3 CLI shape — NO query string — is still a global switch,
        // and its success body carries no `client` key.
        let response = http
            .post(format!("{base}/_ccm/switch/modely"))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let view: Value = response.json().await.unwrap();
        assert_eq!(view["target"], "modely");
        assert!(view.get("client").is_none());

        // Scoped switch over the wire: `?client=` lands in the entry.
        let response = http
            .post(format!("{base}/_ccm/switch/modelx?client=A"))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let view: Value = response.json().await.unwrap();
        assert_eq!(view["client"], "A");
        assert_eq!(view["target"], "modelx");

        // Invalid charset over the wire is a 400, changing nothing.
        let response = http
            .post(format!("{base}/_ccm/switch/modelx?client=bad%20id"))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);

        // /_ccm/clients is a registered route and lists exactly the entry.
        let response = http
            .get(format!("{base}/_ccm/clients"))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let clients: Value = response.json().await.unwrap();
        assert_eq!(clients.as_array().unwrap().len(), 1);
        assert_eq!(clients[0]["client"], "A");
        assert_eq!(clients[0]["target"], "modelx");

        // /_ccm/status: `?client=` shows the effective view; without the
        // parameter the v0.3 shape is unchanged (global view, no new keys).
        let response = http
            .get(format!("{base}/_ccm/status?client=A"))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let view: Value = response.json().await.unwrap();
        assert_eq!(view["client"], "A");
        assert_eq!(view["follows_global"], false);
        assert_eq!(view["target"], "modelx");
        let response = http
            .get(format!("{base}/_ccm/status"))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let view: Value = response.json().await.unwrap();
        assert_eq!(view["target"], "modely"); // the global target
        assert!(view.get("client").is_none());
        assert!(view.get("follows_global").is_none());

        // End to end through the mounted forward path: a wire request with
        // `x-ccm-client: A` routes to A's entry (mock X) and the identity
        // header never reaches the provider; a request WITHOUT the header
        // follows the global target (mock Y).
        let x_before = x_mock.requests.lock().unwrap().len();
        let y_before = y_mock.requests.lock().unwrap().len();
        let response = http
            .post(format!("{base}/v1/messages"))
            .header("content-type", "application/json")
            .header("x-ccm-client", "A")
            .body(
                r#"{"model":"ccm","max_tokens":16,"messages":[{"role":"user","content":"ping"}]}"#,
            )
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(x_mock.requests.lock().unwrap().len(), x_before + 1);
        {
            let requests = x_mock.requests.lock().unwrap();
            let captured = requests.last().unwrap();
            assert_eq!(captured.body["model"], "upstream-x");
            assert!(captured.headers.get("x-ccm-client").is_none());
        }
        let response = http
            .post(format!("{base}/v1/messages"))
            .header("content-type", "application/json")
            .body(
                r#"{"model":"ccm","max_tokens":16,"messages":[{"role":"user","content":"ping"}]}"#,
            )
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(y_mock.requests.lock().unwrap().len(), y_before + 1);

        router_task.abort();
        x_task.abort();
        y_task.abort();
        std::env::remove_var("CCM_HOME");
        std::env::remove_var("CCM_PX_API_KEY");
        std::env::remove_var("CCM_PY_API_KEY");
        let _ = std::fs::remove_dir_all(root);
    }
}
