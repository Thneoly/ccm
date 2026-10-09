use std::{
    collections::{HashMap, VecDeque},
    convert::Infallible,
    net::SocketAddr,
    pin::Pin,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex,
    },
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
    history::{self, History, HistoryLimits, MetricsSnapshot},
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
    usage::{UsageMeta, UsageRecord, UsageStream},
};

#[derive(Clone)]
pub(crate) struct ProxyState {
    pub(crate) client: Client,
    pub(crate) target: Arc<RwLock<String>>,
    /// Per-client runtime targets (v0.4 M3). Entries are created ONLY by a
    /// scoped switch — an unknown client id follows the global target and
    /// never materializes an entry. Since v0.5 M2 the map is seeded from
    /// `clients.toml` at startup and persisted best-effort at scoped
    /// switches and by the refresh task (invariant 8, reworded: the GLOBAL
    /// switch stays runtime-only; scoped entries persist to clients.toml —
    /// never state.toml).
    pub(crate) clients: Arc<RwLock<HashMap<String, ClientEntry>>>,
    /// Persistence for those entries (v0.5 M2). `persist = false` makes
    /// every save/load a no-op — v0.4 memory-only semantics.
    pub(crate) clients_store: Arc<crate::clients_store::ClientsStore>,
    pub(crate) traces: Arc<RwLock<VecDeque<AttemptTrace>>>,
    pub(crate) circuits: Arc<RwLock<HashMap<String, CircuitState>>>,
    pub(crate) metrics: Arc<RwLock<HashMap<String, ModelMetrics>>>,
    pub(crate) decisions: Arc<RwLock<VecDeque<RoutingDecision>>>,
    pub(crate) decision_seq: Arc<AtomicU64>,
    /// History persistence handle (v0.4 M5). `History::disabled()` in tests
    /// and whenever the single-writer lock cannot be acquired — every
    /// `record_*` is then a no-op, so no call site branches on it.
    pub(crate) history: History,
    /// In-memory usage-record ring (v0.4 M6), newest last, capped at
    /// [`crate::usage::USAGE_CAPACITY`]. A plain `Mutex`, not the tokio
    /// `RwLock` used by the neighbors: the finalize path runs synchronously
    /// inside `poll_next`/`Drop` on the response body, where awaiting is
    /// impossible.
    pub(crate) usage: Arc<Mutex<VecDeque<UsageRecord>>>,
    /// Prometheus exporter state (v0.4 M7). `None` = `prometheus_enabled =
    /// false` — the `/metrics` route is then not registered and every hook
    /// above is a no-op, mirroring how a disabled `History` works.
    pub(crate) prom: Option<Arc<crate::prometheus::PromState>>,
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

    // History store (v0.4 M5): best-effort persistence. Any failure to
    // resolve the directory or acquire the single-writer lock downgrades to
    // a disabled store with one warning — observability must never prevent
    // routing. Decision ids continue the persisted sequence so history stays
    // unique across restarts sharing one CCM_HOME.
    let observability = config.observability.clone();
    let (history, decision_seq_start) = if observability.history_enabled {
        match AppConfig::history_dir() {
            Ok(dir) => match history::open_history(
                dir.clone(),
                HistoryLimits::from(&observability),
                history::CHANNEL_CAPACITY,
            ) {
                Ok(store) => {
                    let start = history::recover_decision_seq(&dir);
                    println!(
                        "History: {} (decisions, metric snapshots, circuit transitions)",
                        dir.display()
                    );
                    (store, start)
                }
                Err(err) => {
                    eprintln!("ccm: history disabled: {err:#}");
                    (History::disabled(), 1)
                }
            },
            Err(err) => {
                eprintln!("ccm: history disabled: {err:#}");
                (History::disabled(), 1)
            }
        }
    } else {
        // Explicit config choice — still worth one stderr line so the
        // missing `History:` startup line has a visible explanation.
        eprintln!("ccm: history disabled by [observability] config (history_enabled = false)");
        (History::disabled(), 1)
    };

    // Pin the process-start gauge before the listener exists (v0.4 M7).
    crate::prometheus::note_process_start();
    let prom = observability
        .prometheus_enabled
        .then(|| Arc::new(crate::prometheus::PromState::new()));

    // Persistent client sessions (v0.5 M2): load BEFORE the state exists so
    // restored entries seed the map. Every problem degrades to a dropped
    // entry with one stderr note — persistence must never prevent routing.
    let clients_store = Arc::new(crate::clients_store::ClientsStore::from_config(&config)?);
    let sessions = clients_store.load(&config, now_ms());
    for note in &sessions.notes {
        eprintln!("ccm: {note}");
    }
    let mut restored_clients = sessions.clients;
    // A hand-grown file over the cap trims to the cap here too.
    clients_store.enforce_cap(&mut restored_clients);
    if clients_store.is_enabled() {
        println!(
            "Clients: {} persisted session(s) from {}",
            restored_clients.len(),
            clients_store.path().display()
        );
    }

    let state = ProxyState {
        client: Client::new(),
        target: Arc::new(RwLock::new(target)),
        clients: Arc::new(RwLock::new(restored_clients)),
        clients_store: clients_store.clone(),
        traces: Arc::new(RwLock::new(VecDeque::with_capacity(TRACE_CAPACITY))),
        circuits: Arc::new(RwLock::new(HashMap::new())),
        metrics: Arc::new(RwLock::new(HashMap::new())),
        decisions: Arc::new(RwLock::new(VecDeque::with_capacity(DECISION_CAPACITY))),
        decision_seq: Arc::new(AtomicU64::new(decision_seq_start)),
        history,
        usage: Arc::new(Mutex::new(VecDeque::with_capacity(
            crate::usage::USAGE_CAPACITY,
        ))),
        prom,
    };

    // Periodic whole-state metric snapshots (v0.4 M5). Skipped while the
    // metrics map is empty so an idle proxy writes nothing. No exit-time
    // final snapshot is claimed — the proxy has no graceful shutdown; the
    // periodic snapshots plus torn-tail tolerance cover the gap.
    if state.history.is_enabled() {
        let snapshot_state = state.clone();
        let interval = Duration::from_secs(observability.metrics_snapshot_interval_secs);
        tokio::spawn(async move {
            loop {
                sleep(interval).await;
                let models = snapshot_state.metrics.read().await.clone();
                if models.is_empty() {
                    continue;
                }
                snapshot_state
                    .history
                    .record_metrics_snapshot(&MetricsSnapshot {
                        timestamp_ms: now_ms(),
                        models: models.into_iter().collect(),
                    });
            }
        });
    }

    // Periodic clients.toml refresh (v0.5 M2): rewrite the file with the
    // map's CURRENT state so traffic-bumped `last_seen_ms` values land on
    // disk. Those values are persisted UNCHANGED — this task never stamps
    // now() (that would make the TTL dead code; the switch path stamps at
    // real switch events, the hot path at real requests). Idle-skip while
    // the map is empty; `persist` serializes against the switch path's
    // writes and skips a byte-identical body, so idle ticks touch no
    // disk. Throttled warn-on-failure, no exit-time write — the
    // metrics-snapshot task's conventions.
    if clients_store.is_enabled() {
        let refresh_state = state.clone();
        let refresh_store = clients_store.clone();
        let interval = Duration::from_secs(crate::clients_store::REFRESH_INTERVAL_SECS);
        tokio::spawn(async move {
            let mut warned = false;
            loop {
                sleep(interval).await;
                if refresh_state.clients.read().await.is_empty() {
                    continue;
                }
                match refresh_store.persist(&refresh_state.clients).await {
                    Ok(()) => warned = false,
                    Err(err) => {
                        if !warned {
                            eprintln!(
                                "ccm: clients.toml write failed: {err:#} (warning shown once per failure streak)"
                            );
                            warned = true;
                        }
                    }
                }
            }
        });
    }

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
// CCM refuses to expose them beyond the local machine. Remote/LAN binding
// needs an authentication design first (still an open backlog item).
fn ensure_loopback(addr: SocketAddr) -> Result<()> {
    if !addr.ip().is_loopback() {
        bail!("refusing to bind non-loopback address {addr}: the CCM control API is unauthenticated, remote binding is not supported");
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
// count_tokens forwarding (v0.5 M3 / V0.5_PLAN §3.3)
// ===========================================================================

/// How long the WHOLE count_tokens upstream call — response headers and
/// the buffered body — may take before the proxy gives up. Counting is
/// fast, and the shared proxy client carries no timeout of its own; the
/// buffered passthrough has no streaming escape hatch the way /v1/messages
/// does, so a gateway that answers headers then stalls mid-body must still
/// hit this bound.
const COUNT_TOKENS_TIMEOUT_SECS: u64 = 10;

pub(crate) async fn forward_count_tokens(
    State(state): State<ProxyState>,
    request: Request<Body>,
) -> impl IntoResponse {
    match count_tokens(state, request).await {
        Ok(response) => response,
        Err(err) => (StatusCode::BAD_GATEWAY, format!("CCM proxy error: {err:#}")).into_response(),
    }
}

/// `POST /v1/messages/count_tokens`: forward the count to the PRIMARY
/// model's upstream, single attempt, touching NOTHING else — no
/// circuit/metric/decision/trace/usage/history side effects (a count body
/// has no `usage` object; count traffic is deliberately invisible to
/// every metric family — the documented boundary). The target read is
/// side-effect-free too ([`peek_request_target`]: counters and last_seen
/// stay generation-driven).
async fn count_tokens(state: ProxyState, request: Request<Body>) -> Result<Response<Body>> {
    if request.method() != Method::POST {
        return Ok((StatusCode::METHOD_NOT_ALLOWED, "POST required").into_response());
    }

    let client_id = client_id_from_headers(request.headers());
    let target = peek_request_target(&state, client_id.as_deref()).await;
    let config = AppConfig::load().context("failed to reload CCM config")?;
    let route = config.resolve_route(&target)?;
    // PRIMARY only: a count is meaningful for the model that will
    // actually serve generation — fallbacks never enter the picture.
    let model = config
        .models
        .get(&route.primary)
        .with_context(|| format!("route primary model `{}` is not configured", route.primary))?;
    let provider = config
        .providers
        .get(&model.provider)
        .with_context(|| format!("provider `{}` is not configured", model.provider))?;

    // openai-compatible primary: the chat-completions protocol has no
    // counting endpoint, so there is nothing to forward to. Refuse with
    // the status the client demonstrably already tolerates (M0: today's
    // bare 404s were survived) — never a local estimate from the wrong
    // tokenizer.
    let Some(count_path) = provider.kind.count_tokens_path() else {
        return chunked_json_response(
            StatusCode::NOT_FOUND,
            serde_json::to_vec(&count_tokens_refusal_body())
                .context("failed to serialize count_tokens refusal")?,
            None,
        );
    };

    let token = credential::get(&model.provider)?;
    let (parts, body) = request.into_parts();
    let bytes = axum::body::to_bytes(body, 16 * 1024 * 1024)
        .await
        .context("failed to read request body")?;
    // The model rewrite is protocol-agnostic and mandatory: Claude Code
    // sends the placeholder `ccm` model name.
    let body = rewrite_model(bytes, &model.model_id)?;

    let upstream = format!("{}{}", provider.base_url.trim_end_matches('/'), count_path);
    let mut builder = state.client.post(upstream).body(body);
    builder = copy_request_headers(builder, &parts.headers, provider.kind);
    builder = provider.apply_auth(builder, &token);

    // The bound covers the WHOLE upstream call — headers AND the buffered
    // body (verify-pass fix: a send-only timeout let a headers-then-stall
    // gateway hang the handler indefinitely).
    let (status, headers, bytes) = timeout(Duration::from_secs(COUNT_TOKENS_TIMEOUT_SECS), async {
        let response = builder
            .send()
            .await
            .context("count_tokens upstream request failed")?;
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = response
            .bytes()
            .await
            .context("failed to read count_tokens response body")?;
        Ok::<_, anyhow::Error>((status, headers, bytes))
    })
    .await
    .map_err(|_| anyhow::anyhow!("count_tokens timed out after {COUNT_TOKENS_TIMEOUT_SECS}s"))??;

    // Status + body passthrough, byte-identical: a gateway that lacks the
    // endpoint answers itself — exactly the client's pre-CCM experience.

    warn_once_on_zero_count(status.is_success(), &bytes, bytes_len_of(&parts));

    let mut response = Response::builder().status(status.as_u16());
    for (name, value) in headers.iter() {
        if is_hop_by_hop(name.as_str()) || name == header::CONTENT_LENGTH {
            continue;
        }
        response = response.header(name, value);
    }
    response
        .body(Body::from(bytes))
        .context("failed to build count_tokens response")
}

/// Anthropic error envelope for the openai-compatible count_tokens
/// refusal — the `translation_failure_body` shape with a boundary-naming
/// message.
fn count_tokens_refusal_body() -> Value {
    serde_json::json!({
        "type": "error",
        "error": {
            "type": "not_found_error",
            "message": "ccm: the primary model for this target is openai-compatible; its protocol has no count_tokens endpoint and no local estimate is attempted",
        }
    })
}

/// One process-lifetime stderr note for the stub-gateway shape (M0
/// finding: z.ai answers `{"input_tokens":0}` to any input). A zero count
/// on a large request passes through — indistinguishable from a legal
/// empty-input count — but the operator should hear about it once.
static WARNED_ZERO_COUNT: AtomicBool = AtomicBool::new(false);
fn warn_once_on_zero_count(success: bool, response_body: &[u8], request_len: usize) {
    if !success || WARNED_ZERO_COUNT.load(Ordering::Relaxed) {
        return;
    }
    let zero = serde_json::from_slice::<Value>(response_body)
        .ok()
        .and_then(|value| value.get("input_tokens").and_then(Value::as_u64))
        == Some(0);
    // 8 KiB is comfortably above any small-prompt count body, so this
    // only fires for genuinely large requests a stub gateway zeroed.
    if zero && request_len > 8192 {
        eprintln!(
            "ccm: count_tokens upstream answered input_tokens=0 for a {request_len}-byte request — a stub gateway; the count passes through unchanged"
        );
        WARNED_ZERO_COUNT.store(true, Ordering::Relaxed);
    }
}

/// Best-effort request-body size from the original request's framing (the
/// bytes were consumed by the rewrite; the header survives).
fn bytes_len_of(parts: &axum::http::request::Parts) -> usize {
    parts
        .headers
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse().ok())
        .unwrap_or(0)
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

/// Side-effect-free counterpart of [`resolve_request_target`] (v0.5 M3):
/// the same client-id resolution and entry-or-global target choice, but a
/// plain READ — `requests` and `last_seen_ms` are not bumped, because a
/// count_tokens call is not generation traffic and the counters stay
/// generation-driven (the shared §3.2/§3.3 decision, V0.5_PLAN §8.3).
async fn peek_request_target(state: &ProxyState, client: Option<&str>) -> String {
    let global = state.target.read().await.clone();
    match client {
        Some(id) => state
            .clients
            .read()
            .await
            .get(id)
            .map(|entry| entry.target.clone())
            .unwrap_or(global),
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
            // Terminal fallback-status passthrough: an error body, no usage.
            return proxy_response(upstream_response, None);
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
        // Usage capture (v0.4 M6): wrap the ACCEPTED body only. The
        // decision id must be read before `store_decision` moves it.
        let usage = usage_context(
            &state,
            &config,
            decision.id,
            candidate,
            client_id.as_deref(),
            status.is_success(),
        );
        store_decision(&state, decision).await;
        if matches!(provider.kind, ProviderKind::OpenAICompatible) {
            if !status.is_success() {
                // A non-fallback error status (400/401/500 ...) is terminal:
                // translate the OpenAI error body, preserve the status code.
                return translated_error_response(upstream_response).await;
            }
            if client_wants_stream && response_is_event_stream(&upstream_response) {
                return translated_sse_response(upstream_response, &model_id, usage.as_ref());
            }
            return translated_json_response(upstream_response, &model_id, usage.as_ref()).await;
        }
        return proxy_response(upstream_response, usage.as_ref());
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

// ===========================================================================
// usage capture wiring (v0.4 M6)
// ===========================================================================

/// What the unified response dispatch point hands to the usage wrapper for
/// one accepted response. Built per terminal attempt in `forward()`;
/// `None` on the error passthrough paths (terminal fallback status,
/// translated error bodies) where no accepted body is worth scanning. The
/// pricing snapshot is resolved here, at capture time, so a config edit
/// mid-flight never rewrites an already-emitted record.
struct UsageContext {
    meta: UsageMeta,
    history: History,
    ring: Arc<Mutex<VecDeque<UsageRecord>>>,
    /// Exporter hooks for the same finalize event (v0.4 M7): token counters
    /// and accumulated cost. `None` = exporter disabled.
    prom: Option<Arc<crate::prometheus::PromState>>,
}

impl UsageContext {
    /// Wrap a response-body stream with the usage scanner; `sse` selects
    /// line scanning (true) vs whole-body JSON scan (false). The record is
    /// emitted on stream end, transport error, or wrapper Drop (client
    /// disconnect) — see [`UsageStream`]. The ring push, the history
    /// enqueue, and the exporter bump are all synchronous and non-blocking
    /// by design.
    fn wrap<E: Send + 'static>(
        &self,
        inner: Pin<Box<dyn Stream<Item = Result<Bytes, E>> + Send>>,
        sse: bool,
    ) -> UsageStream<E> {
        let meta = self.meta.clone();
        let history = self.history.clone();
        let ring = Arc::clone(&self.ring);
        let prom = self.prom.clone();
        UsageStream::new(inner, sse, meta, move |record| {
            {
                let mut ring = ring.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                if ring.len() >= crate::usage::USAGE_CAPACITY {
                    ring.pop_front();
                }
                ring.push_back(record.clone());
            }
            history.record_usage(&record);
            if let Some(prom) = &prom {
                prom.record_usage(
                    &record.model,
                    record.input_tokens,
                    record.output_tokens,
                    record.cache_read_tokens,
                    record.cache_write_tokens,
                    record.cost_usd,
                );
            }
        })
    }
}

/// Build the usage capture context for one terminal attempt, or `None`
/// when this response is not an accepted 2xx body. Kept next to the
/// dispatch call sites so the accepted/error split stays visible.
fn usage_context(
    state: &ProxyState,
    config: &AppConfig,
    decision_id: u64,
    candidate: &str,
    client: Option<&str>,
    accepted: bool,
) -> Option<UsageContext> {
    if !accepted {
        return None;
    }
    Some(UsageContext {
        meta: UsageMeta {
            decision_id,
            model: candidate.to_string(),
            client: client.map(str::to_string),
            // Snapshot, not a reference into config: the body may outlive
            // this request's config reload.
            pricing: config
                .models
                .get(candidate)
                .and_then(|model| model.pricing.clone()),
        },
        history: state.history.clone(),
        ring: Arc::clone(&state.usage),
        prom: state.prom.clone(),
    })
}

fn proxy_response(
    upstream_response: reqwest::Response,
    usage: Option<&UsageContext>,
) -> Result<Response<Body>> {
    let status = upstream_response.status();
    let headers = upstream_response.headers().clone();
    let sse = response_is_event_stream(&upstream_response);
    let stream = upstream_response.bytes_stream();

    let mut response = Response::builder().status(status.as_u16());
    for (name, value) in headers.iter() {
        if is_hop_by_hop(name.as_str()) || name == header::CONTENT_LENGTH {
            continue;
        }
        response = response.header(name, value);
    }

    let boxed: Pin<Box<dyn Stream<Item = reqwest::Result<Bytes>> + Send>> = Box::pin(stream);
    let body = match usage {
        // Native path: SSE vs JSON follows the actual upstream content-type.
        Some(ctx) => Body::from_stream(ctx.wrap(boxed, sse)),
        None => Body::from_stream(boxed),
    };
    response
        .body(body)
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
    usage: Option<&UsageContext>,
) -> Result<Response<Body>> {
    let status = upstream_response.status();
    let stream = TranslatedSseStream {
        upstream: Box::pin(upstream_response.bytes_stream()),
        translator: SseTranslator::new(model_id),
        pending: VecDeque::new(),
        finished: false,
    };
    let boxed: Pin<Box<dyn Stream<Item = reqwest::Result<Bytes>> + Send>> = Box::pin(stream);
    let body = match usage {
        // The wire body is the TRANSLATED anthropic event stream — that is
        // what the scanner sees, so message_delta carries the real usage.
        Some(ctx) => Body::from_stream(ctx.wrap(boxed, true)),
        None => Body::from_stream(boxed),
    };
    Response::builder()
        .status(status.as_u16())
        .header(header::CONTENT_TYPE, "text/event-stream")
        .body(body)
        .context("failed to build translated streaming response")
}

/// Accepted non-streaming response from an openai-compatible upstream: buffer
/// the body once and translate it to an Anthropic message JSON. The commit
/// point was the response headers, so a translation failure does not retry or
/// fall back; it surfaces a 502 Anthropic error envelope instead.
async fn translated_json_response(
    upstream_response: reqwest::Response,
    model_id: &str,
    usage: Option<&UsageContext>,
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
                // an error body is not a billable response
                None,
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
            // the translated anthropic envelope carries the usage object
            usage,
        ),
        Err(err) => chunked_json_response(
            StatusCode::BAD_GATEWAY,
            serde_json::to_vec(&translation_failure_body(&format!("{err:#}")))
                .context("failed to serialize error response")?,
            None,
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
        // terminal error passthrough: no usage capture
        None,
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
/// non-streaming paths so the body framing matches `proxy_response`. A
/// `usage` context (accepted bodies only) wraps the single chunk with the
/// usage scanner.
fn chunked_json_response(
    status: StatusCode,
    body: Vec<u8>,
    usage: Option<&UsageContext>,
) -> Result<Response<Body>> {
    let boxed: Pin<Box<dyn Stream<Item = Result<Bytes, Infallible>> + Send>> =
        Box::pin(OnceStream::new(body));
    let body = match usage {
        Some(ctx) => Body::from_stream(ctx.wrap(boxed, false)),
        None => Body::from_stream(boxed),
    };
    Response::builder()
        .status(status.as_u16())
        .header(header::CONTENT_TYPE, "application/json")
        .body(body)
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
        control_traces, ClientParams, DecisionParams,
    };
    use crate::routing::decision::{now_ms, DecisionCandidate};
    use axum::extract::{Path, Query};
    use axum::{routing::post, Router};
    use serde_json::json;

    /// All five integration tests mutate process env (`CCM_HOME`,
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

    #[derive(Clone)]
    enum MockBehavior {
        OkStream,
        Status(StatusCode),
        DelayOk(u64),
        // 200 + text/event-stream carrying a REAL native usage contract:
        // message_start with input/cache tokens, message_delta with output
        // (v0.4 M6 usage-capture fixtures).
        OkStreamWithUsage,
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
            MockBehavior::OkStreamWithUsage => stream_ok_with_usage(),
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

    /// Native anthropic stream with the full usage contract: start carries
    /// input + cache_read + cache_write, the final delta carries output.
    fn stream_ok_with_usage() -> Response<Body> {
        Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "text/event-stream")
            .body(Body::from(concat!(
                "event: message_start\n",
                "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":100,\"cache_read_input_tokens\":40,\"cache_creation_input_tokens\":5}}}\n\n",
                "event: content_block_delta\n",
                "data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}\n\n",
                "event: message_delta\n",
                "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":7}}\n\n",
                "event: message_stop\n",
                "data: {\"type\":\"message_stop\"}\n\n",
            )))
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
            .route("/v1/messages/count_tokens", post(mock_messages))
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
            // Memory-only store: integration tests keep v0.4 semantics
            // unless a test builds its own persisting store.
            clients_store: Arc::new(crate::clients_store::ClientsStore::disabled()),
            traces: Arc::new(RwLock::new(VecDeque::with_capacity(TRACE_CAPACITY))),
            circuits: Arc::new(RwLock::new(HashMap::new())),
            metrics: Arc::new(RwLock::new(HashMap::new())),
            decisions: Arc::new(RwLock::new(VecDeque::with_capacity(DECISION_CAPACITY))),
            decision_seq: Arc::new(AtomicU64::new(1)),
            history: History::disabled(),
            usage: Arc::new(std::sync::Mutex::new(VecDeque::with_capacity(
                crate::usage::USAGE_CAPACITY,
            ))),
            // Exporter on in the integration harness so /metrics and the
            // request hooks are exercised by the wire-level tests.
            prom: Some(Arc::new(crate::prometheus::PromState::new())),
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

    /// Shared fixture config. `kind` picks the protocol pair — the openai
    /// variant reuses the same two mock upstreams but both providers speak
    /// `chat/completions`. Pricing tables (v0.4 M6) ride along on both
    /// models so usage-capture tests can assert real cost math; routing
    /// itself never reads them.
    #[allow(clippy::too_many_arguments)] // test fixture writer, args mirror the config knobs
    fn write_integration_config_with_kind(
        root: &std::path::Path,
        primary_url: &str,
        fallback_url: &str,
        selection: &str,
        header_timeout_ms: u64,
        failure_threshold: usize,
        open_ms: u64,
        kind: &str,
    ) {
        std::fs::create_dir_all(root).unwrap();
        let raw = format!(
            r#"
[providers.primary]
kind = "{kind}"
base_url = "{primary_url}"
auth = "x-api-key"

[providers.fallback]
kind = "{kind}"
base_url = "{fallback_url}"
auth = "bearer"

[models.primary]
provider = "primary"
model_id = "upstream-primary"

[models.primary.routing]
cost_weight = 10.0
quality_weight = 0.2

[models.primary.pricing]
input = 3.0
output = 15.0
cache_read = 0.3
cache_write = 3.75

[models.fallback]
provider = "fallback"
model_id = "upstream-fallback"

[models.fallback.routing]
cost_weight = 0.1
quality_weight = 0.9

[models.fallback.pricing]
input = 1.0
output = 2.0

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

    fn write_integration_config(
        root: &std::path::Path,
        primary_url: &str,
        fallback_url: &str,
        selection: &str,
        header_timeout_ms: u64,
        failure_threshold: usize,
        open_ms: u64,
    ) {
        write_integration_config_with_kind(
            root,
            primary_url,
            fallback_url,
            selection,
            header_timeout_ms,
            failure_threshold,
            open_ms,
            "anthropic-compatible",
        );
    }

    async fn body_text(response: Response<Body>) -> String {
        let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    /// Decode a `/_ccm/decisions` handler response into its records. The
    /// handler returns `impl IntoResponse` (in-memory, disk-backed, or error
    /// branches), so tests read the JSON body instead of a typed return.
    async fn decisions_from(response: impl IntoResponse) -> Vec<RoutingDecision> {
        let text = body_text(response.into_response()).await;
        serde_json::from_str(&text).unwrap()
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
        let usage_before = state.usage.lock().unwrap().len();
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
        // The usage record on this buffered path carries the REAL token
        // counts (prompt 5 / completion 3), not honest zeros: scanner mode
        // follows the actual response content-type, so a JSON body answering
        // a stream:true request is read by the JSON scanner. Pins the M6-era
        // backlog note (V0.4_PLAN section 11) as verified-not-a-defect — it
        // described the pre-6e58889 dispatch that fed JSON bodies to the
        // SSE line scanner. No [pricing] table on this model: cost stays
        // unknown, never guessed.
        let records = state.usage.lock().unwrap().clone();
        assert_eq!(records.len(), usage_before + 1);
        let record = records.back().unwrap();
        assert_eq!(record.model, "openai");
        assert_eq!(
            (
                record.input_tokens,
                record.output_tokens,
                record.cache_read_tokens,
                record.cache_write_tokens
            ),
            (5, 3, 0, 0)
        );
        assert!(record.complete);
        assert_eq!(record.pricing, None);
        assert_eq!(record.cost_usd, None);

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

        let decisions_b = decisions_from(
            control_decisions(
                State(state.clone()),
                Query(DecisionParams {
                    client: Some("B".to_string()),
                    ..DecisionParams::default()
                }),
            )
            .await,
        )
        .await;
        assert_eq!(decisions_b.len(), 3);
        assert!(decisions_b.iter().all(|d| d.client.as_deref() == Some("B")));
        let decisions_all = decisions_from(
            control_decisions(
                State(state.clone()),
                Query(DecisionParams {
                    client: None,
                    ..DecisionParams::default()
                }),
            )
            .await,
        )
        .await;
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

    // v0.5 M2 acceptance: a scoped switch survives a proxy restart. The
    // "restart" is the exact seed serve() performs — load, revalidate, cap,
    // fresh state — against the file the switch itself persisted. Also pins:
    // the requests counter restarts from 0, a no-id request still follows
    // the global target, state.toml stays byte-identical (invariant 9), the
    // file carries no credential material (invariant 1), and a refresh-style
    // save persists last_seen values UNCHANGED (idle entries keep their
    // stamp — never now()), which keeps the TTL meaningful.
    #[allow(clippy::await_holding_lock)] // see note on the v0.3 test above
    #[tokio::test]
    async fn scoped_switches_survive_a_restart_via_clients_toml() {
        let _env_guard = env_guard();
        let root = std::env::temp_dir().join(format!(
            "ccm-clients-restart-{}-{}",
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
        // invariant 9's pinned artifact: state.toml is never touched by a
        // scoped switch, before or after the restart.
        std::fs::write(root.join("state.toml"), "current = \"modelx\"\n").unwrap();
        let state_toml_before = std::fs::read(root.join("state.toml")).unwrap();

        // "proxy A": the default-enabled store; two scoped switches.
        let config = AppConfig::load().unwrap();
        let store_a = crate::clients_store::ClientsStore::from_config(&config).unwrap();
        assert!(store_a.is_enabled());
        let mut state_a = integration_proxy_state();
        state_a.clients_store = Arc::new(store_a.clone());
        apply_switch(&state_a, "modely", Some("term1"))
            .await
            .unwrap();
        apply_switch(&state_a, "modelx", Some("term2"))
            .await
            .unwrap();
        let switch_stamp_term2 = state_a
            .clients
            .read()
            .await
            .get("term2")
            .unwrap()
            .last_seen_ms;

        // The switch path persisted synchronously: both entries on disk.
        let file_text = std::fs::read_to_string(root.join("clients.toml")).unwrap();
        assert!(file_text.contains("id = \"term1\""), "{file_text}");
        assert!(file_text.contains("target = \"modely\""), "{file_text}");
        assert!(!file_text.contains("x-secret") && !file_text.contains("y-secret"));

        // kill -9 equivalent: everything from proxy A is dropped. The
        // restart seeds the map exactly the way serve() does.
        drop(state_a);
        let config = AppConfig::load().unwrap();
        let store_b = crate::clients_store::ClientsStore::from_config(&config).unwrap();
        let sessions = store_b.load(&config, now_ms());
        assert!(sessions.notes.is_empty(), "{:?}", sessions.notes);
        let mut restored = sessions.clients;
        store_b.enforce_cap(&mut restored);
        let mut state_b = integration_proxy_state();
        state_b.clients = Arc::new(RwLock::new(restored));
        state_b.clients_store = Arc::new(store_b.clone());
        *state_b.target.write().await = "modelx".to_string();

        // both entries restored; the requests counter restarted from 0
        {
            let clients = state_b.clients.read().await;
            let term1 = clients.get("term1").unwrap();
            assert_eq!(term1.target, "modely");
            assert_eq!(term1.requests, 0);
            assert_eq!(clients.get("term2").unwrap().target, "modelx");
        }

        // one request with the id routes to the SCOPED target (mock Y); the
        // no-id request still follows the global target (mock X)
        let x_before = x_mock.requests.lock().unwrap().len();
        let y_before = y_mock.requests.lock().unwrap().len();
        let response = forward(state_b.clone(), client_tagged_request(Some("term1"), None))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(y_mock.requests.lock().unwrap().len(), y_before + 1);
        assert_eq!(x_mock.requests.lock().unwrap().len(), x_before);
        let response = forward(state_b.clone(), integration_request())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(x_mock.requests.lock().unwrap().len(), x_before + 1);

        // A refresh-style persist (what the periodic task does) persists
        // the map's CURRENT last_seen values UNCHANGED: the traffic-bumped
        // term1 carries its bumped stamp, the idle term2 keeps its switch
        // stamp exactly — nothing is re-stamped with now().
        store_b.persist(&state_b.clients).await.unwrap();
        let file_text = std::fs::read_to_string(root.join("clients.toml")).unwrap();
        let idle_stamp_line = format!("last_seen_ms = {switch_stamp_term2}");
        assert!(
            file_text.contains(&idle_stamp_line),
            "idle entry keeps its stamp: {file_text}"
        );
        assert!(
            !file_text.contains(&format!("last_seen_ms = {}", now_ms())),
            "no now()-stamping on refresh: {file_text}"
        );
        assert!(!file_text.contains("x-secret") && !file_text.contains("y-secret"));

        // /_ccm/clients shape after restart: requests counted from 0, one
        // routed request on term1
        let clients = control_clients(State(state_b.clone())).await.0;
        let term1_row = clients.iter().find(|row| row.client == "term1").unwrap();
        assert_eq!(term1_row.requests, 1);

        assert_eq!(
            state_toml_before,
            std::fs::read(root.join("state.toml")).unwrap()
        );

        x_task.abort();
        y_task.abort();
        std::env::remove_var("CCM_HOME");
        std::env::remove_var("CCM_PX_API_KEY");
        std::env::remove_var("CCM_PY_API_KEY");
        let _ = std::fs::remove_dir_all(root);
    }

    // `[clients] persist = false` restores v0.4 memory-only semantics: the
    // entry exists in memory, no clients.toml is ever created.
    #[allow(clippy::await_holding_lock)] // see note on the v0.3 test above
    #[tokio::test]
    async fn persist_false_restores_memory_only_semantics() {
        let _env_guard = env_guard();
        let root = std::env::temp_dir().join(format!(
            "ccm-clients-memonly-{}-{}",
            std::process::id(),
            now_ms()
        ));
        std::env::set_var("CCM_HOME", &root);
        let (x_addr, _x_mock, x_task) = spawn_mock(MockBehavior::OkStream).await;
        let (y_addr, _y_mock, y_task) = spawn_mock(MockBehavior::OkStream).await;
        write_multi_client_config(
            &root,
            &format!("http://{x_addr}"),
            &format!("http://{y_addr}"),
        );
        std::fs::write(
            root.join("config.toml"),
            std::fs::read_to_string(root.join("config.toml")).unwrap()
                + "\n[clients]\npersist = false\n",
        )
        .unwrap();

        let config = AppConfig::load().unwrap();
        let store = crate::clients_store::ClientsStore::from_config(&config).unwrap();
        assert!(!store.is_enabled());
        let mut state = integration_proxy_state();
        state.clients_store = Arc::new(store);
        apply_switch(&state, "modely", Some("term1")).await.unwrap();
        assert_eq!(
            state.clients.read().await.get("term1").unwrap().target,
            "modely"
        );
        assert!(!root.join("clients.toml").exists());

        x_task.abort();
        y_task.abort();
        std::env::remove_var("CCM_HOME");
        let _ = std::fs::remove_dir_all(root);
    }

    // v0.5 M3: count_tokens forwarding. The count goes to the PRIMARY of
    // the effective target (scoped entry or global), single attempt, body
    // byte-identical except the model rewrite; the response passes through
    // unchanged; every routing/observability map is bit-identical
    // before/after (including the /metrics exposition); the scoped entry's
    // counters are NOT bumped; non-POST is a 405; an openai-compatible
    // primary gets the 404 refusal envelope without an upstream call.
    #[allow(clippy::await_holding_lock)] // see note on the v0.3 test above
    #[tokio::test]
    async fn count_tokens_forwarding_covers_the_contract() {
        use crate::control::api::control_metrics_exposition;

        let _env_guard = env_guard();
        let root = std::env::temp_dir().join(format!(
            "ccm-count-tokens-{}-{}",
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
        // Scoped A -> modely (mock Y); its counters are captured BEFORE any
        // count traffic to pin the side-effect-free read.
        apply_switch(&state, "modely", Some("A")).await.unwrap();
        let entry_before = state.clients.read().await.get("A").unwrap().clone();
        let circuits_before = state.circuits.read().await.clone();
        let metrics_before = state.metrics.read().await.clone();
        let decisions_before = state.decisions.read().await.clone();
        let traces_before = state.traces.read().await.clone();
        let usage_before = state.usage.lock().unwrap().clone();
        let prom_before = body_text(control_metrics_exposition(State(state.clone())).await).await;

        *x_mock.behavior.write().await =
            MockBehavior::OpenaiJson(r#"{"input_tokens":7}"#.to_string());
        *y_mock.behavior.write().await =
            MockBehavior::OpenaiJson(r#"{"input_tokens":42}"#.to_string());

        let count_request = |client: Option<&str>| {
            let mut builder = Request::builder()
                .method(Method::POST)
                .uri("/v1/messages/count_tokens")
                .header(header::CONTENT_TYPE, "application/json");
            if let Some(id) = client {
                builder = builder.header("x-ccm-client", id);
            }
            builder
                .body(Body::from(
                    r#"{"model":"ccm","messages":[{"role":"user","content":"ping"}]}"#,
                ))
                .unwrap()
        };

        // Scoped: A's entry target (modely -> mock Y), model rewritten,
        // bearer credential injected, identity stripped, response
        // byte-identical.
        let response = forward_count_tokens(State(state.clone()), count_request(Some("A")))
            .await
            .into_response();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(body_text(response).await, r#"{"input_tokens":42}"#);
        {
            let requests = y_mock.requests.lock().unwrap();
            let captured = requests.last().unwrap();
            assert_eq!(captured.path, "/v1/messages/count_tokens");
            assert_eq!(captured.body["model"], "upstream-y");
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
        assert_eq!(x_mock.requests.lock().unwrap().len(), 0);

        // Global (no id): the global target's primary (modelx -> mock X).
        let response = forward_count_tokens(State(state.clone()), count_request(None))
            .await
            .into_response();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(body_text(response).await, r#"{"input_tokens":7}"#);
        {
            let requests = x_mock.requests.lock().unwrap();
            let captured = requests.last().unwrap();
            assert_eq!(captured.path, "/v1/messages/count_tokens");
            assert_eq!(captured.body["model"], "upstream-x");
        }

        // An unknown id follows the global target, creating no entry.
        let response = forward_count_tokens(State(state.clone()), count_request(Some("ghost")))
            .await
            .into_response();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(x_mock.requests.lock().unwrap().len(), 2);
        assert!(state.clients.read().await.get("ghost").is_none());

        // Zero side effects: every map bit-identical, A's counters unbumped.
        assert_eq!(*state.circuits.read().await, circuits_before);
        assert_eq!(*state.metrics.read().await, metrics_before);
        assert_eq!(*state.decisions.read().await, decisions_before);
        assert_eq!(*state.traces.read().await, traces_before);
        assert_eq!(*state.usage.lock().unwrap(), usage_before);
        let prom_after = body_text(control_metrics_exposition(State(state.clone())).await).await;
        assert_eq!(prom_after, prom_before, "count traffic touches no metric");
        let entry_after = state.clients.read().await.get("A").unwrap().clone();
        assert_eq!(entry_after.requests, entry_before.requests);
        assert_eq!(entry_after.last_seen_ms, entry_before.last_seen_ms);

        // Non-POST is the same 405 shape /v1/messages serves.
        let get = Request::builder()
            .method(Method::GET)
            .uri("/v1/messages/count_tokens")
            .body(Body::empty())
            .unwrap();
        let response = forward_count_tokens(State(state.clone()), get)
            .await
            .into_response();
        assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);

        // An upstream that lacks the endpoint answers itself: status and
        // body pass through unchanged (the pre-CCM client experience).
        *x_mock.behavior.write().await = MockBehavior::Status(StatusCode::NOT_FOUND);
        let response = forward_count_tokens(State(state.clone()), count_request(None))
            .await
            .into_response();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(body_text(response).await, "mock 404 Not Found");

        // The 10s bound covers the WHOLE upstream call, not just headers: a
        // gateway that answers 200 then stalls mid-body (held-open stream,
        // never EOF) must surface the 502 timeout instead of hanging the
        // client indefinitely (verify-pass regression pin).
        *x_mock.behavior.write().await =
            MockBehavior::OpenaiSseHeldOpen(r#"{"input_tokens":1}"#.to_string());
        let started = std::time::Instant::now();
        let response = forward_count_tokens(State(state.clone()), count_request(None))
            .await
            .into_response();
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        assert!(
            body_text(response).await.contains("timed out after"),
            "the timeout names its bound"
        );
        assert!(
            started.elapsed() >= Duration::from_secs(COUNT_TOKENS_TIMEOUT_SECS),
            "the bound fired on the whole call"
        );

        // openai-compatible primary: the 404 refusal envelope, no upstream
        // call, no local estimate.
        let (openai_addr, openai_mock, openai_task) = spawn_mock(MockBehavior::OkStream).await;
        let (native_addr, _native_mock, native_task) = spawn_mock(MockBehavior::OkStream).await;
        write_openai_integration_config(
            &root,
            &format!("http://{openai_addr}"),
            &format!("http://{native_addr}"),
        );
        std::env::set_var("CCM_OPENAI_API_KEY", "openai-secret");
        std::env::set_var("CCM_NATIVE_API_KEY", "native-secret");
        *state.target.write().await = "test-route".to_string();
        let response = forward_count_tokens(State(state.clone()), count_request(None))
            .await
            .into_response();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let refusal: Value = serde_json::from_str(&body_text(response).await).unwrap();
        assert_eq!(refusal["type"], "error");
        assert_eq!(refusal["error"]["type"], "not_found_error");
        assert!(refusal["error"]["message"]
            .as_str()
            .unwrap()
            .contains("openai-compatible"));
        assert_eq!(openai_mock.requests.lock().unwrap().len(), 0);
        // The scoped entry also resolves through its target's primary: re-point
        // A at the openai route (the rewritten config no longer defines its
        // original modely target) and the refusal is identical.
        apply_switch(&state, "test-route", Some("A")).await.unwrap();
        let response = forward_count_tokens(State(state.clone()), count_request(Some("A")))
            .await
            .into_response();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);

        x_task.abort();
        y_task.abort();
        openai_task.abort();
        native_task.abort();
        std::env::remove_var("CCM_HOME");
        std::env::remove_var("CCM_PX_API_KEY");
        std::env::remove_var("CCM_PY_API_KEY");
        std::env::remove_var("CCM_OPENAI_API_KEY");
        std::env::remove_var("CCM_NATIVE_API_KEY");
        let _ = std::fs::remove_dir_all(root);
    }

    // v0.4 M5: routing decisions, metric snapshots, and circuit transitions
    // persist to $CCM_HOME/history/*.jsonl while the proxy runs, decision ids
    // stay unique, and the persisted lines never contain credential material
    // (V0.4_PLAN §10, invariant 1). Uses a REAL history store — single-writer
    // lock included — against the mock routing stack.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn history_persistence_integration_covers_jsonl_contract() {
        let _env_guard = env_guard();
        let root = std::env::temp_dir().join(format!(
            "ccm-history-integration-{}-{}",
            std::process::id(),
            now_ms()
        ));
        std::env::set_var("CCM_HOME", &root);
        std::env::set_var("CCM_PRIMARY_API_KEY", "primary-secret");
        std::env::set_var("CCM_FALLBACK_API_KEY", "fallback-secret");

        let history_dir = AppConfig::history_dir().unwrap();
        let store = history::open_history(
            history_dir.clone(),
            HistoryLimits::default(),
            history::CHANNEL_CAPACITY,
        )
        .unwrap();
        let mut state = ProxyState {
            client: Client::new(),
            target: Arc::new(RwLock::new("test-route".to_string())),
            clients: Arc::new(RwLock::new(HashMap::new())),
            clients_store: Arc::new(crate::clients_store::ClientsStore::disabled()),
            traces: Arc::new(RwLock::new(VecDeque::with_capacity(TRACE_CAPACITY))),
            circuits: Arc::new(RwLock::new(HashMap::new())),
            metrics: Arc::new(RwLock::new(HashMap::new())),
            decisions: Arc::new(RwLock::new(VecDeque::with_capacity(DECISION_CAPACITY))),
            decision_seq: Arc::new(AtomicU64::new(history::recover_decision_seq(&history_dir))),
            history: store,
            usage: Arc::new(std::sync::Mutex::new(VecDeque::with_capacity(
                crate::usage::USAGE_CAPACITY,
            ))),
            prom: Some(Arc::new(crate::prometheus::PromState::new())),
        };

        let (primary_addr, primary, primary_task) = spawn_mock(MockBehavior::OkStream).await;
        let (fallback_addr, fallback, fallback_task) = spawn_mock(MockBehavior::OkStream).await;
        let primary_url = format!("http://{primary_addr}");
        let fallback_url = format!("http://{fallback_addr}");
        let _ = &fallback; // captured requests are not needed here; files are
        write_integration_config(&root, &primary_url, &fallback_url, "ordered", 250, 1, 30);
        let mut request = integration_request();
        request.headers_mut().insert(
            "x-ccm-client",
            axum::http::HeaderValue::from_static("term1"),
        );
        let response = forward(state.clone(), request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        // 2. one 503 at failure_threshold=1 opens the circuit
        //    (CLOSED→OPEN transition persists); the next request skips
        //    primary and succeeds on fallback.
        *primary.behavior.write().await = MockBehavior::Status(StatusCode::SERVICE_UNAVAILABLE);
        let response = forward(state.clone(), integration_request()).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let primary_count_after_open = primary.requests.lock().unwrap().len();

        let response = forward(state.clone(), integration_request()).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            primary.requests.lock().unwrap().len(),
            primary_count_after_open,
            "circuit OPEN skipped primary"
        );

        // 3. cooldown elapsed → probe admitted (OPEN→HALF_OPEN) → success
        //    closes the circuit (HALF_OPEN→CLOSED). The probe body carries
        //    a REAL usage contract so the M6 capture path persists too —
        //    draining the body is what runs the scanner.
        sleep(Duration::from_millis(40)).await;
        *primary.behavior.write().await = MockBehavior::OkStreamWithUsage;
        let response = forward(state.clone(), integration_request()).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert!(body_text(response).await.contains("message_stop"));

        // 4. one metric snapshot through the real handle (the serve()
        //    snapshot task is not running in this test; the handle path is
        //    identical).
        state.history.record_metrics_snapshot(&MetricsSnapshot {
            timestamp_ms: now_ms(),
            models: state.metrics.read().await.clone().into_iter().collect(),
        });

        // Assignment drops the real store: Drop joins the writer after a
        // final drain+flush, making the reads below deterministic.
        state.history = History::disabled();

        // Persisted decisions: ids strictly increasing from 1 on an empty
        // store, client id retained, circuit-skip attempt sequence retained.
        let decisions =
            history::read_decisions(&history_dir, &crate::history::DecisionQuery::default())
                .unwrap();
        assert!(decisions.len() >= 4);
        assert_eq!(decisions[0].id, 1, "seq starts at 1 on an empty store");
        assert!(
            decisions.windows(2).all(|pair| pair[0].id < pair[1].id),
            "ids strictly increasing"
        );
        assert!(
            decisions
                .iter()
                .any(|d| d.client.as_deref() == Some("term1")),
            "client id persisted on the decision"
        );
        assert!(
            decisions
                .iter()
                .any(|d| d.attempts.iter().any(|a| a.result.contains("circuit OPEN"))),
            "the circuit-skip attempt sequence persisted"
        );

        // The full circuit lifecycle persisted, model-scoped to `primary`.
        let transitions = history::read_circuit_transitions(&history_dir, None, None);
        assert!(transitions
            .iter()
            .any(|t| t.from == "CLOSED" && t.to == "OPEN"));
        assert!(transitions
            .iter()
            .any(|t| t.from == "OPEN" && t.to == "HALF_OPEN"));
        assert!(transitions
            .iter()
            .any(|t| t.from == "HALF_OPEN" && t.to == "CLOSED"));
        assert!(transitions.iter().all(|t| t.model == "primary"));

        // The snapshot round-trips with live counters.
        let snapshots = history::read_metrics_snapshots(&history_dir, Some(1));
        assert_eq!(snapshots.len(), 1);
        assert!(snapshots[0].models.contains_key("primary"));
        assert!(snapshots[0].models.contains_key("fallback"));
        assert!(snapshots[0].models["primary"].attempts >= 2);

        // usage.jsonl (v0.4 M6): the drained probe body became one record,
        // joined to its decision id, with the pricing snapshot and cost.
        // (The dedicated reader lands with the query endpoints; this pins
        // the on-disk contract by parsing the lines directly.)
        let usage_text =
            std::fs::read_to_string(history_dir.join("usage.jsonl")).expect("usage.jsonl");
        let usage_records: Vec<crate::usage::UsageRecord> = usage_text
            .lines()
            .map(|line| serde_json::from_str(line).expect("usage line parses"))
            .collect();
        assert_eq!(
            usage_records.len(),
            1,
            "one record per drained accepted body"
        );
        let record = &usage_records[0];
        assert_eq!(record.model, "primary");
        assert_eq!(
            (
                record.input_tokens,
                record.output_tokens,
                record.cache_read_tokens,
                record.cache_write_tokens
            ),
            (100, 7, 40, 5)
        );
        assert!(record.complete);
        assert!(
            decisions.iter().any(|d| d.id == record.decision_id),
            "usage joins a persisted decision id"
        );
        assert_eq!(record.pricing.as_ref().map(|p| p.input), Some(3.0));
        let expected = (100.0 * 3.0 + 7.0 * 15.0 + 40.0 * 0.3 + 5.0 * 3.75) / 1_000_000.0;
        assert!((record.cost_usd.unwrap() - expected).abs() < 1e-12);

        // Credential invariant (V0.4_PLAN §10, invariant 1): history files
        // are whitelist serde structs; no key material ever appears.
        for name in [
            "decisions.jsonl",
            "metrics.jsonl",
            "circuit.jsonl",
            "usage.jsonl",
        ] {
            let contents = std::fs::read_to_string(history_dir.join(name))
                .unwrap_or_else(|err| panic!("reading {name}: {err}"));
            assert!(
                !contents.contains("primary-secret") && !contents.contains("fallback-secret"),
                "{name} leaked credential material"
            );
        }

        primary_task.abort();
        fallback_task.abort();
        std::env::remove_var("CCM_HOME");
        std::env::remove_var("CCM_PRIMARY_API_KEY");
        std::env::remove_var("CCM_FALLBACK_API_KEY");
        let _ = std::fs::remove_dir_all(&root);
    }

    // v0.4 M6 usage capture: one record per accepted 2xx response. Native
    // anthropic stream (start+delta merge, pricing snapshot, cost math,
    // decision-id join), translated openai stream (real usage rides the
    // delta), and the terminal error passthrough (no capture at all).
    // History stays disabled here — the disk path is pinned by the
    // persistence test above; this one pins the ring + capture semantics.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn usage_capture_integration_covers_native_and_translated_streams() {
        let _env_guard = env_guard();
        let root = std::env::temp_dir().join(format!(
            "ccm-usage-integration-{}-{}",
            std::process::id(),
            now_ms()
        ));
        std::env::set_var("CCM_HOME", &root);
        std::env::set_var("CCM_PRIMARY_API_KEY", "primary-secret");
        std::env::set_var("CCM_FALLBACK_API_KEY", "fallback-secret");

        let (primary_addr, primary, primary_task) =
            spawn_mock(MockBehavior::OkStreamWithUsage).await;
        let (fallback_addr, fallback, fallback_task) = spawn_mock(MockBehavior::OkStream).await;
        let primary_url = format!("http://{primary_addr}");
        let fallback_url = format!("http://{fallback_addr}");
        let state = integration_proxy_state();
        let _ = &fallback;

        // 1. native anthropic stream: full token set, cost math, decision join
        write_integration_config(&root, &primary_url, &fallback_url, "ordered", 250, 3, 50);
        let response = forward(state.clone(), integration_request()).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        // draining the body is what drives the scanner to finalize
        assert!(body_text(response).await.contains("message_stop"));

        let last_decision_id = state.decisions.read().await.back().unwrap().id;
        let records = state.usage.lock().unwrap().clone();
        assert_eq!(records.len(), 1, "exactly one record per accepted body");
        let record = &records[0];
        assert_eq!(
            record.decision_id, last_decision_id,
            "usage joins the decision"
        );
        assert_eq!(record.model, "primary");
        assert_eq!(record.client, None, "no client id on a plain request");
        assert_eq!(
            (
                record.input_tokens,
                record.output_tokens,
                record.cache_read_tokens,
                record.cache_write_tokens
            ),
            (100, 7, 40, 5)
        );
        assert!(record.complete);
        assert_eq!(record.pricing.as_ref().map(|p| p.input), Some(3.0));
        let expected = (100.0 * 3.0 + 7.0 * 15.0 + 40.0 * 0.3 + 5.0 * 3.75) / 1_000_000.0;
        assert!((record.cost_usd.unwrap() - expected).abs() < 1e-12);

        // 2. translated openai stream: message_start carries zeros; the
        //    real usage (prompt 10, completion 2) rides message_delta
        write_integration_config_with_kind(
            &root,
            &primary_url,
            &fallback_url,
            "ordered",
            250,
            3,
            50,
            "openai-compatible",
        );
        *primary.behavior.write().await = MockBehavior::OpenaiSse(openai_stream_body());
        // the SSE translation branch needs a streaming client request
        let stream_request = Request::builder()
            .method(Method::POST)
            .uri("/v1/messages")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                r#"{"model":"ccm","max_tokens":16,"stream":true,"messages":[{"role":"user","content":"ping"}]}"#,
            ))
            .unwrap();
        let response = forward(state.clone(), stream_request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert!(body_text(response).await.contains("message_stop"));

        let records = state.usage.lock().unwrap().clone();
        assert_eq!(records.len(), 2);
        let record = &records[1];
        assert_eq!(record.model, "primary");
        assert_eq!(
            (
                record.input_tokens,
                record.output_tokens,
                record.cache_read_tokens
            ),
            (10, 2, 0)
        );
        assert!(record.complete);

        // 3. terminal error passthrough (both candidates 429, budget spent):
        //    no accepted body, no usage record
        *primary.behavior.write().await = MockBehavior::Status(StatusCode::TOO_MANY_REQUESTS);
        *fallback.behavior.write().await = MockBehavior::Status(StatusCode::TOO_MANY_REQUESTS);
        let response = forward(state.clone(), integration_request()).await.unwrap();
        assert_eq!(
            response.status(),
            StatusCode::TOO_MANY_REQUESTS,
            "last-attempt 429 passes through"
        );
        body_text(response).await; // drain; still nothing to scan
        let records = state.usage.lock().unwrap();
        assert_eq!(records.len(), 2, "error passthrough captured no usage");

        primary_task.abort();
        fallback_task.abort();
        std::env::remove_var("CCM_HOME");
        std::env::remove_var("CCM_PRIMARY_API_KEY");
        std::env::remove_var("CCM_FALLBACK_API_KEY");
        let _ = std::fs::remove_dir_all(&root);
    }

    // v0.4 M7: the /metrics exposition over real HTTP — same listener as the
    // control API, correct content type, and the hooks fired by forward()
    // (requests_total, attempts_total, header-latency histogram, tokens,
    // cost, circuit gauge) all land in the rendered text. Also pins the
    // disabled path: a state without the exporter registers no /metrics
    // route at all (404), matching `prometheus_enabled = false`.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn prometheus_exposition_integration_covers_hooks_and_route() {
        let _env_guard = env_guard();
        let root = std::env::temp_dir().join(format!(
            "ccm-prom-integration-{}-{}",
            std::process::id(),
            now_ms()
        ));
        std::env::set_var("CCM_HOME", &root);
        std::env::set_var("CCM_PRIMARY_API_KEY", "primary-secret");
        std::env::set_var("CCM_FALLBACK_API_KEY", "fallback-secret");

        let (primary_addr, primary, primary_task) =
            spawn_mock(MockBehavior::OkStreamWithUsage).await;
        let (fallback_addr, fallback, fallback_task) = spawn_mock(MockBehavior::OkStream).await;
        write_integration_config(
            &root,
            &format!("http://{primary_addr}"),
            &format!("http://{fallback_addr}"),
            "ordered",
            250,
            3, // failure threshold high enough that two 429s never open it
            50,
        );
        let state = integration_proxy_state();
        let _ = &fallback;

        // 1. one accepted stream: success everywhere, tokens + cost recorded
        //    (the mock's usage is input 100 / output 7 / cache_read 40 /
        //    cache_write 5 at the config's prices → 435.75 µUSD → 436).
        let response = forward(state.clone(), integration_request()).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert!(body_text(response).await.contains("message_stop"));

        // 2. terminal failure: both candidates 429 → one error request
        //    outcome, one rate_limited attempt per model.
        *primary.behavior.write().await = MockBehavior::Status(StatusCode::TOO_MANY_REQUESTS);
        *fallback.behavior.write().await = MockBehavior::Status(StatusCode::TOO_MANY_REQUESTS);
        let response = forward(state.clone(), integration_request()).await.unwrap();
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        body_text(response).await;

        // Serve the real router and scrape.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let router_state = state.clone();
        let router_task = tokio::spawn(async move {
            axum::serve(listener, control_router(router_state))
                .await
                .unwrap();
        });
        let http = Client::new();
        let response = http.get(format!("{base}/metrics")).send().await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get("content-type")
                .and_then(|value| value.to_str().ok()),
            Some("text/plain; version=0.0.4; charset=utf-8"),
            "the 0.0.4 text-format content type"
        );
        let text = response.text().await.unwrap();
        assert!(text.contains("\nccm_up 1\n"));
        assert!(text
            .contains("ccm_proxy_requests_total{target=\"test-route\",outcome=\"success\"} 1\n"));
        assert!(
            text.contains("ccm_proxy_requests_total{target=\"test-route\",outcome=\"error\"} 1\n")
        );
        assert!(text.contains("ccm_attempts_total{model=\"primary\",outcome=\"success\"} 1\n"));
        assert!(text.contains("ccm_attempts_total{model=\"primary\",outcome=\"rate_limited\"} 1\n"));
        assert!(
            text.contains("ccm_attempts_total{model=\"fallback\",outcome=\"rate_limited\"} 1\n")
        );
        // success + 429 samples on primary; only the 429 on fallback
        assert!(text.contains("ccm_header_latency_seconds_count{model=\"primary\"} 2\n"));
        assert!(text.contains("ccm_header_latency_seconds_count{model=\"fallback\"} 1\n"));
        assert!(text.contains("ccm_decision_duration_seconds_count{target=\"test-route\"} 2\n"));
        assert!(text.contains("ccm_tokens_total{model=\"primary\",kind=\"input\"} 100\n"));
        assert!(text.contains("ccm_tokens_total{model=\"primary\",kind=\"cache_write\"} 5\n"));
        assert!(text.contains("ccm_cost_micro_usd_total{model=\"primary\"} 436\n"));
        assert!(text.contains("ccm_circuit_open{model=\"primary\"} 0\n"));
        assert!(
            text.contains("ccm_circuit_consecutive_failures{model=\"primary\"} 1\n"),
            "one 429 counted, threshold 3 not reached"
        );
        assert!(text.contains("\nccm_history_dropped_total 0\n"));
        router_task.abort();

        // 3. disabled exporter: the route is not registered — 404, the
        //    documented off state for `prometheus_enabled = false`.
        let mut disabled = state.clone();
        disabled.prom = None;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let router_task = tokio::spawn(async move {
            axum::serve(listener, control_router(disabled))
                .await
                .unwrap();
        });
        let response = http.get(format!("{base}/metrics")).send().await.unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        router_task.abort();

        primary_task.abort();
        fallback_task.abort();
        std::env::remove_var("CCM_HOME");
        std::env::remove_var("CCM_PRIMARY_API_KEY");
        std::env::remove_var("CCM_FALLBACK_API_KEY");
        let _ = std::fs::remove_dir_all(&root);
    }

    // v0.4 M5: any of ?since/?until/?model switches /_ccm/decisions from the
    // in-memory ring to a disk read over the persisted history; the filters
    // compose (time × model × client); a history filter without a running
    // store is a 400 naming the likely causes. Explicit directory, no env
    // mutation — this test needs no env lock.
    #[tokio::test]
    async fn decisions_endpoint_reads_disk_when_history_filters_present() {
        let root = std::env::temp_dir().join(format!(
            "ccm-decisions-disk-{}-{}",
            std::process::id(),
            now_ms()
        ));
        let dir = root.join("history");
        let store = history::open_history(
            dir.clone(),
            HistoryLimits::default(),
            history::CHANNEL_CAPACITY,
        )
        .unwrap();
        let record =
            |id: u64, timestamp_ms: u64, model: &str, client: Option<&str>| RoutingDecision {
                id,
                timestamp_ms,
                target: "route".to_string(),
                client: client.map(str::to_string),
                selection: "ordered".to_string(),
                configured_candidates: vec![model.to_string()],
                ranked_candidates: vec![DecisionCandidate {
                    rank: 1,
                    model: model.to_string(),
                    reliability_score: 1.0,
                    latency_score: 1.0,
                    cost_score: 1.0,
                    quality_score: 1.0,
                    weighted_score: 1.0,
                }],
                attempts: vec![DecisionAttempt {
                    attempt: 1,
                    model: model.to_string(),
                    circuit: "CLOSED".to_string(),
                    result: "HTTP 200".to_string(),
                    fallback: false,
                }],
                selected: Some(model.to_string()),
                outcome: "HTTP 200".to_string(),
            };
        store.record_decision(&record(1, 1_000, "alpha", None));
        store.record_decision(&record(2, 2_000, "beta", Some("term1")));
        store.record_decision(&record(3, 3_000, "alpha", None));
        // Deterministic flush: dropping the store joins the writer after a
        // final drain. Reopen on the same directory so the disk-branch reads
        // below still resolve a live history store (same files).
        drop(store);
        let store = history::open_history(
            dir.clone(),
            HistoryLimits::default(),
            history::CHANNEL_CAPACITY,
        )
        .unwrap();

        let mut state = integration_proxy_state();
        state.history = store;
        // The in-memory ring holds DIFFERENT records so the branch choice is
        // observable from what comes back.
        state
            .decisions
            .write()
            .await
            .push_back(record(99, 9_999, "ring-only", None));
        state
            .decisions
            .write()
            .await
            .push_back(record(100, 9_999, "ring-only", None));

        // No history filters: the in-memory ring, exactly as v0.3.
        let ring = decisions_from(
            control_decisions(State(state.clone()), Query(DecisionParams::default())).await,
        )
        .await;
        assert_eq!(
            ring.iter().map(|d| d.id).collect::<Vec<_>>(),
            vec![99, 100],
            "no filters -> the in-memory ring"
        );
        // A client-only filter also stays in memory.
        let ring_client = decisions_from(
            control_decisions(
                State(state.clone()),
                Query(DecisionParams {
                    client: Some("nobody".to_string()),
                    ..DecisionParams::default()
                }),
            )
            .await,
        )
        .await;
        assert!(ring_client.is_empty(), "client filter, still in-memory");

        // A limit without history filters stays in memory and keeps the
        // newest N (it is not silently ignored).
        let ring_limit = decisions_from(
            control_decisions(
                State(state.clone()),
                Query(DecisionParams {
                    limit: Some(1),
                    ..DecisionParams::default()
                }),
            )
            .await,
        )
        .await;
        assert_eq!(
            ring_limit.iter().map(|d| d.id).collect::<Vec<_>>(),
            vec![100],
            "limit alone truncates the in-memory view to the newest N"
        );

        // since=0: the full disk history, oldest first.
        let disk = decisions_from(
            control_decisions(
                State(state.clone()),
                Query(DecisionParams {
                    since: Some(0),
                    ..DecisionParams::default()
                }),
            )
            .await,
        )
        .await;
        assert_eq!(
            disk.iter().map(|d| d.id).collect::<Vec<_>>(),
            vec![1, 2, 3],
            "any history filter -> disk read"
        );

        // Inclusive time window pins the boundary record.
        let window = decisions_from(
            control_decisions(
                State(state.clone()),
                Query(DecisionParams {
                    since: Some(2_000),
                    until: Some(2_000),
                    ..DecisionParams::default()
                }),
            )
            .await,
        )
        .await;
        assert_eq!(window.iter().map(|d| d.id).collect::<Vec<_>>(), vec![2]);

        // Model filter on the disk branch (via selected or attempts).
        let by_model = decisions_from(
            control_decisions(
                State(state.clone()),
                Query(DecisionParams {
                    model: Some("beta".to_string()),
                    ..DecisionParams::default()
                }),
            )
            .await,
        )
        .await;
        assert_eq!(by_model.iter().map(|d| d.id).collect::<Vec<_>>(), vec![2]);

        // Explicit limit on the disk branch keeps the most recent N.
        let limited = decisions_from(
            control_decisions(
                State(state.clone()),
                Query(DecisionParams {
                    since: Some(0),
                    limit: Some(2),
                    ..DecisionParams::default()
                }),
            )
            .await,
        )
        .await;
        assert_eq!(
            limited.iter().map(|d| d.id).collect::<Vec<_>>(),
            vec![2, 3],
            "limit keeps the newest records"
        );

        // Client composes with the time filter on disk reads.
        let composed = decisions_from(
            control_decisions(
                State(state.clone()),
                Query(DecisionParams {
                    since: Some(0),
                    client: Some("term1".to_string()),
                    ..DecisionParams::default()
                }),
            )
            .await,
        )
        .await;
        assert_eq!(
            composed.iter().map(|d| d.id).collect::<Vec<_>>(),
            vec![2],
            "client filter composes on the disk branch"
        );

        // A history filter without a running store is a 400 that names the
        // likely causes (config off or single-writer lock lost).
        let response = control_decisions(
            State(integration_proxy_state()), // History::disabled()
            Query(DecisionParams {
                since: Some(0),
                ..DecisionParams::default()
            }),
        )
        .await
        .into_response();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert!(body_text(response).await.contains("history_enabled"));

        drop(state); // release the single-writer lock before cleanup
        let _ = std::fs::remove_dir_all(&root);
    }

    // v0.4 M6: /_ccm/usage mirrors the decisions dual-branch contract
    // (in-memory ring without history filters, bounded disk read with any),
    // and /_ccm/cost?day= aggregates one UTC day by model — priced and
    // unpriced separated, complete counted. Explicit directory, no env
    // mutation — this test needs no env lock.
    #[tokio::test]
    async fn usage_and_cost_endpoints_cover_disk_contract() {
        use crate::control::api::{control_cost, control_usage, CostParams, UsageParams};
        use crate::model::ModelPricing;
        use crate::usage::{UsageMeta, UsageRecord};

        let root = std::env::temp_dir().join(format!(
            "ccm-usage-endpoints-{}-{}",
            std::process::id(),
            now_ms()
        ));
        let dir = root.join("history");
        let store = history::open_history(
            dir.clone(),
            HistoryLimits::default(),
            history::CHANNEL_CAPACITY,
        )
        .unwrap();

        // Records on two UTC days. Timestamps are pinned relative to the
        // real clock so the default-?day= branch is also exercised
        // deterministically: "today" records land inside today's window
        // whatever the local timezone.
        let today_start =
            crate::date::parse_utc_day(&crate::date::utc_day_of(crate::date::now_ms()))
                .expect("utc_day_of always produces a parseable day");
        let record = |id: u64, timestamp_ms: u64, model: &str, client: Option<&str>| {
            let meta = UsageMeta {
                decision_id: id,
                model: model.to_string(),
                client: client.map(str::to_string),
                pricing: (model == "glm").then_some(ModelPricing {
                    input: 3.0,
                    output: 15.0,
                    cache_read: 0.3,
                    cache_write: 3.75,
                }),
            };
            let mut scanner = crate::usage::UsageScanner::new(false);
            scanner.feed(br#"{"usage":{"input_tokens":100,"output_tokens":7,"cache_read_input_tokens":40,"cache_creation_input_tokens":5}}"#);
            let mut record = UsageRecord::from_capture(meta, scanner.finish(true));
            record.timestamp_ms = timestamp_ms;
            record
        };
        store.record_usage(&record(1, today_start + 1_000, "glm", Some("term1")));
        store.record_usage(&record(2, today_start + 2_000, "glm", Some("term2")));
        store.record_usage(&record(3, today_start + 3_000, "deepseek", None));
        // yesterday: a day filter must exclude it
        store.record_usage(&record(4, today_start - 1, "glm", None));
        // a disconnected stream today: counted, but not complete
        let mut incomplete = record(5, today_start + 4_000, "glm", None);
        incomplete.complete = false;
        store.record_usage(&incomplete);
        drop(store); // deterministic flush

        let store = history::open_history(
            dir.clone(),
            HistoryLimits::default(),
            history::CHANNEL_CAPACITY,
        )
        .unwrap();
        let mut state = integration_proxy_state();
        state.history = store;
        // The ring holds a DIFFERENT record so the branch choice is
        // observable from what comes back.
        state
            .usage
            .lock()
            .unwrap()
            .push_back(record(99, today_start + 9_999, "ring-only", None));

        // --- /_ccm/usage: no history filters -> the in-memory ring.
        let ring =
            usage_from(control_usage(State(state.clone()), Query(UsageParams::default())).await)
                .await;
        assert_eq!(
            ring.iter().map(|r| r.decision_id).collect::<Vec<_>>(),
            vec![99],
            "no filters -> the in-memory ring"
        );
        // client-only filter also stays in memory (and may match nothing).
        let ring_client = usage_from(
            control_usage(
                State(state.clone()),
                Query(UsageParams {
                    client: Some("nobody".to_string()),
                    ..UsageParams::default()
                }),
            )
            .await,
        )
        .await;
        assert!(ring_client.is_empty());

        // --- /_ccm/usage: any history filter -> disk, oldest first.
        let disk = usage_from(
            control_usage(
                State(state.clone()),
                Query(UsageParams {
                    since: Some(0),
                    ..UsageParams::default()
                }),
            )
            .await,
        )
        .await;
        assert_eq!(
            disk.iter().map(|r| r.decision_id).collect::<Vec<_>>(),
            vec![1, 2, 3, 4, 5]
        );
        // model + since compose on the disk branch.
        let disk_model = usage_from(
            control_usage(
                State(state.clone()),
                Query(UsageParams {
                    since: Some(0),
                    model: Some("deepseek".to_string()),
                    ..UsageParams::default()
                }),
            )
            .await,
        )
        .await;
        assert_eq!(disk_model.len(), 1);
        assert_eq!(disk_model[0].model, "deepseek");

        // --- /_ccm/cost?day=: today's aggregation, priced vs unpriced
        // separated, complete counted, yesterday excluded.
        let today = crate::date::utc_day_of(crate::date::now_ms());
        let response = control_cost(
            State(state.clone()),
            Query(CostParams {
                day: Some(today.clone()),
                client: None,
            }),
        )
        .await
        .into_response();
        assert_eq!(response.status(), StatusCode::OK);
        let view: Value = serde_json::from_str(&body_text(response).await).unwrap();
        assert_eq!(view["day"], today.as_str());
        assert_eq!(view["since"], json!(today_start));
        assert_eq!(view["until"], json!(today_start + 86_400_000 - 1));
        assert_eq!(view["requests"], json!(4), "yesterday's record excluded");
        assert_eq!(
            view["complete"],
            json!(3),
            "the disconnected stream is not complete"
        );
        let models = view["models"].as_array().unwrap();
        assert_eq!(models.len(), 2, "glm + deepseek");
        assert_eq!(models[0]["model"], "deepseek", "rows sorted by name");
        assert_eq!(models[0]["requests"], json!(1));
        assert_eq!(models[0]["unpriced_requests"], json!(1));
        assert_eq!(models[0]["cost_usd"], json!(0.0), "unpriced adds zero");
        assert_eq!(models[1]["model"], "glm");
        assert_eq!(models[1]["requests"], json!(3));
        assert_eq!(models[1]["unpriced_requests"], json!(0));
        // glm cost math: 3 × (100×3.0 + 7×15.0 + 40×0.3 + 5×3.75) / 1e6
        let expected = 3.0 * (100.0 * 3.0 + 7.0 * 15.0 + 40.0 * 0.3 + 5.0 * 3.75) / 1_000_000.0;
        let glm_cost = models[1]["cost_usd"].as_f64().unwrap();
        assert!((glm_cost - expected).abs() < 1e-12);
        assert_eq!(view["total_unpriced_requests"], json!(1));
        let total = view["total_cost_usd"].as_f64().unwrap();
        assert!((total - expected).abs() < 1e-12, "total = glm only");

        // client filter narrows the day: term1's one glm record.
        let response = control_cost(
            State(state.clone()),
            Query(CostParams {
                day: Some(today.clone()),
                client: Some("term1".to_string()),
            }),
        )
        .await
        .into_response();
        let view: Value = serde_json::from_str(&body_text(response).await).unwrap();
        assert_eq!(view["requests"], json!(1));
        assert_eq!(view["client"], "term1");
        assert_eq!(view["models"][0]["requests"], json!(1));

        // a different day: only yesterday's record.
        let yesterday = crate::date::utc_day_of(today_start - 1);
        let response = control_cost(
            State(state.clone()),
            Query(CostParams {
                day: Some(yesterday),
                client: None,
            }),
        )
        .await
        .into_response();
        let view: Value = serde_json::from_str(&body_text(response).await).unwrap();
        assert_eq!(view["requests"], json!(1));

        // malformed day and a missing store are 400s naming the cause.
        let response = control_cost(
            State(state.clone()),
            Query(CostParams {
                day: Some("2026-02-30".to_string()),
                client: None,
            }),
        )
        .await
        .into_response();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert!(body_text(response).await.contains("invalid ?day"));
        let response = control_cost(
            State(integration_proxy_state()), // History::disabled()
            Query(CostParams::default()),
        )
        .await
        .into_response();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert!(body_text(response).await.contains("history_enabled"));
        // ... and so is a history-filtered /_ccm/usage without a store.
        let response = control_usage(
            State(integration_proxy_state()),
            Query(UsageParams {
                since: Some(0),
                ..UsageParams::default()
            }),
        )
        .await
        .into_response();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert!(body_text(response).await.contains("history_enabled"));

        drop(state); // release the single-writer lock before cleanup
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Decode a `/_ccm/usage` handler response into its records (same shape
    /// as `decisions_from`).
    async fn usage_from(response: impl IntoResponse) -> Vec<crate::usage::UsageRecord> {
        let text = body_text(response.into_response()).await;
        serde_json::from_str(&text).unwrap()
    }
}
