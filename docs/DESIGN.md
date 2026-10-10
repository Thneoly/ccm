# CCM Design

## 1. Product Goal

CCM is a local model-routing control plane for AI coding CLIs, starting with Claude Code.

The v0.3 goal is deliberately narrow:

> Keep the coding CLI attached to one stable local endpoint while CCM controls model/provider selection, runtime switching, fallback, health-aware routing, circuit breaking, metrics, and routing explanations behind that endpoint.

CCM is not intended to become a general API gateway in v0.3.

## 2. Design Principles

1. **Local-first control plane**
   - Configuration, credentials, runtime state, routing metrics, and traces stay local.
   - No CCM cloud service is required.

2. **Stable client attachment**
   - Claude Code connects to CCM once.
   - Backend changes happen behind the local proxy.

3. **Separate declaration from mutable state**
   - `config.toml`: declarative providers, models, profiles, routes, policies.
   - `state.toml`: persisted mutable default target.
   - Proxy runtime target: in-memory only.

4. **Explicit provider behavior**
   - Provider endpoint and authentication style are explicit configuration.
   - CCM does not infer bearer vs x-api-key from provider names.

5. **Explainable routing**
   - Dynamic routing decisions must be inspectable.
   - No opaque "AI chose this model" behavior.

6. **Circuit breaker is a hard admission gate**
   - Selection ranks candidates.
   - Circuit breaker decides whether a ranked candidate is allowed to execute.

7. **Streaming must remain streaming**
   - CCM does not buffer a completed model response merely to collect metrics.
   - Latency metrics use time-to-response-headers.

8. **Feature freeze after Routing Decision Trace**
   - v0.3 should now optimize correctness, testability, portability, and release quality.

## 3. Architecture

```text
Claude Code
    |
    | Anthropic-compatible /v1/messages
    v
+---------------------------+
|        CCM Proxy          |
+---------------------------+
    |
    v
Runtime Target
(model/profile/route)
    |
    v
Route Resolution
    |
    v
Candidate Selector
    |-- ordered
    |-- healthiest
    |-- lowest-latency
    |-- lowest-cost
    `-- weighted
    |
    v
Circuit Breaker
    |-- CLOSED
    |-- OPEN
    `-- HALF_OPEN
    |
    v
Policy Executor
    |-- header timeout
    |-- retry budget
    |-- fallback statuses
    `-- backoff
    |
    v
Provider + Credential
    |
    v
Upstream /v1/messages
```

Side channels:

```text
Proxy execution
   |----> Attempt Trace
   |----> Model Metrics
   |----> Circuit State
   |----> Routing Decision Trace
   `----> Control API
```

## 4. Domain Model

### Provider

A Provider describes endpoint/protocol/authentication behavior.

```toml
[providers.anthropic]
kind = "anthropic"
base_url = "https://api.anthropic.com"
auth = "x-api-key"

[providers.deepseek]
kind = "openai-compatible"
base_url = "https://api.deepseek.com"
```

Current provider kinds:

- `anthropic` — native Anthropic `/v1/messages` upstream
- `anthropic-compatible` — Anthropic-protocol gateway upstream
- `openai-compatible` — OpenAI `chat/completions` upstream; CCM translates
  the Anthropic client protocol both ways (v0.4 M2)

Current authentication styles:

- `x-api-key`
- `bearer`

`auth` may be omitted; the default resolves by kind: `x-api-key` for
`anthropic` and `anthropic-compatible`, `bearer` for `openai-compatible`.
An explicit `auth` always wins.

Important invariant:

> Exactly one upstream credential header is injected by CCM.

CCM strips inbound `x-api-key` and `Authorization` before forwarding and then injects the configured provider credential. For `openai-compatible` providers, the inbound `anthropic-version` header is also stripped (it is meaningless upstream).

#### Provider discovery (v0.5 M5)

`ccm discover [provider]` lists a gateway's `GET {base_url}/v1/models`
(kind-appropriate auth via `apply_auth`, `anthropic-version` on anthropic
kinds, 10s timeout) and registers selected ids as models. Parsing is
deliberately lenient across the three observed shapes — the common
skeleton is `data[].id`; the Anthropic list shape adds `has_more`/
`first_id`/`last_id` pagination and `display_name`; the OpenAI shape adds
`object: "list"` / `owned_by`; every extra field is optional (z.ai omits
pagination entirely). Cursor pagination follows `has_more` + `after_id`
up to 10 pages, first request carries no query.

Registration contract: an already-registered `(provider, model_id)` pair
is SKIPPED with a notice, never overwritten — discovery must not clobber
hand-tuned weights or prices (`ccm add model` remains the upsert path).
New models land with default routing weights and `pricing = None`; a
commented `[models.<alias>.pricing]` skeleton is printed per selected
unpriced model (prices are hand-entered TOML facts, by policy). Alias =
normalized model_id (`[A-Za-z0-9_-]` kept, everything else `-`),
collision-suffixed against existing config names — model aliases and
profile names, across providers — and within the run (a mechanical
alias must never capture a profile target).

Degradation paths are honest exits, not guesses: 404/405 → "add models
manually"; 401/403 → the health-style auth failure text; a 200 body
with no `data` member → "not a models list" carrying the gateway's own
words (z.ai answers a bad key as HTTP 200 + `{"code":401,...}` — the
auth failure must not degrade to a misleading "0 models"); a shape/kind
mismatch (anthropic shape on an openai kind or vice versa) warns on
stderr but requests still follow the declared kind. The discovery
client follows NO redirects — the credential is keyed to the configured
host (reqwest strips `Authorization` on cross-host redirects but not
`x-api-key`), so a 302 ingress surfaces as its own status — and every
gateway body quoted into an error is scrubbed of the sent token first
(invariant 1: a proxy echoing `x-api-key: ...` back in a diagnostic
body never reaches the terminal). The gateway's list
is a convenience, not a contract: listed ids may still 400 at use time,
and working models may be unlisted. A provider whose base_url points at
another CCM gets the 404 path today (the proxy does not serve
`/v1/models`).

### Model

A Model binds a logical alias to one Provider and one upstream model ID.

```toml
[models.claude]
provider = "anthropic"
model_id = "claude-sonnet-5-5"

[models.claude.routing]
cost_weight = 1.0
quality_weight = 1.0
```

Routing metadata is static user-provided metadata:

- `cost_weight`: lower means cheaper.
- `quality_weight`: user-provided relative quality prior.

These are not measured billing or benchmark values.

### Profile

Profiles are legacy one-model aliases.

```toml
[profiles.coding]
model = "claude"
```

Profiles remain supported for compatibility but Route is the preferred routing abstraction.

### Route

A Route contains:

- primary model
- ordered fallback models
- RoutePolicy

```toml
[routes.coding-route]
primary = "claude"
fallback = ["glm"]

[routes.coding-route.policy]
selection = "weighted"
header_timeout_ms = 30000
fallback_on = [429, 502, 503, 504]
max_attempts = 3
backoff_ms = 200
```

### RoutePolicy

RoutePolicy owns execution and selection behavior.

Current fields:

- `selection`
- `weights`
- `header_timeout_ms`
- `fallback_on`
- `max_attempts`
- `backoff_ms`
- `circuit_breaker`

Backward compatibility is handled with serde defaults.

## 5. Candidate Selection

Supported strategies are frozen for v0.3:

- `ordered`
- `healthiest`
- `lowest-latency`
- `lowest-cost`
- `weighted`

### ordered

Preserves route configuration order.

### healthiest

Uses observed success rate after at least three real attempts.

Before three samples, the candidate uses neutral reliability rank `1.0`.

### lowest-latency

Uses response-header latency EWMA.

EWMA alpha:

```text
0.2
```

Unmeasured candidates sort behind measured candidates.

### lowest-cost

Uses ascending `model.routing.cost_weight`.

### weighted

Current score:

```text
reliability_score = success_rate, or 1.0 before 3 samples
latency_score     = 1 / (1 + latency_ewma_ms / 1000)
cost_score        = 1 / (1 + model.cost_weight)
quality_score     = clamp(model.quality_weight, 0, 1)

weighted_score =
(
  reliability_weight * reliability_score
+ latency_weight     * latency_score
+ cost_weight        * cost_score
+ quality_weight     * quality_score
)
/
sum(weights)
```

Default route weights:

```text
reliability = 0.4
latency     = 0.2
cost        = 0.2
quality     = 0.2
```

## 6. Circuit Breaker

Circuit state is keyed by **model alias**, not Route.

Reason:

> A failing upstream model should be recognized as unhealthy even when reused by multiple Routes.

State machine:

```text
CLOSED
   |
   | consecutive failures >= threshold
   v
OPEN
   |
   | cooldown elapsed
   v
HALF_OPEN
   |-- success --> CLOSED
   `-- failure --> OPEN
```

Only one HALF_OPEN probe is admitted at a time.

The `/_ccm/circuits` control view reports `HALF_OPEN_READY` once the OPEN cooldown has elapsed but before the next HALF_OPEN probe is admitted.

Other concurrent requests skip that candidate and continue routing.

A circuit skip does **not** consume `max_attempts`.

Health failures currently include:

- request/connect errors
- response-header timeout
- HTTP status configured in `fallback_on`

Non-fallback HTTP statuses do not increment circuit failure state.

## 7. Fallback Semantics

Execution order:

1. Rank candidates.
2. For each candidate:
   - enforce attempt budget
   - ask circuit breaker for admission
   - resolve model/provider/credential
   - rewrite model ID
   - send upstream request
3. Fallback on:
   - request/connect error
   - response-header timeout
   - configured fallback HTTP status
4. Stream accepted response back unchanged except hop-by-hop/content-length headers.

Important semantics:

- Request body is buffered once, with a 16 MiB limit.
- Model ID is rewritten per attempt.
- Credentials are resolved per attempt.
- No mid-stream failover.
- Header timeout ends once response headers arrive.
- Accepted response body remains streamed.

## 8. Metrics

Metrics are in-memory and keyed by model alias.

Current metrics:

- attempts
- successes
- success_rate
- health_score
- http_errors
- fallback_failures
- timeouts
- request_errors
- rate_limited
- latency_ewma_ms
- last_success_ms
- last_failure_ms

Current health score:

```text
health_score = success_rate * 100
```

### Prometheus export (v0.4 M7)

The same counters are exported in the Prometheus text format at
`GET /metrics` on the proxy's ONE listener — the loopback bind guard covers
it; there is no second port. Hand-rendered exposition (~150 lines), no
exporter crate. `[observability] prometheus_enabled = false` (default true)
removes the `/metrics` ROUTE entirely. Since v0.5 M4 the route is keyed on
`prometheus_enabled` ALONE, while the exporter state behind it exists
whenever EITHER export surface is on — an OTLP-only proxy (push on, scrape
off) keeps the counters alive for its push loop with the route still
unregistered.

### OTLP/HTTP JSON push export (v0.5 M4)

The second exit for the same counters: `src/otlp.rs` (the
dependency-discipline extension of `prometheus.rs` — hand-rolled
ExportMetricsServiceRequest JSON, zero new dependencies) renders the very
same `Snapshot` the `/metrics` handler serves — `assemble_snapshot` in
`control/api.rs` is the ONE shared input, so the two surfaces cannot
disagree — and a `serve()` push task POSTs it to
`{[observability.otlp].endpoint}/v1/metrics` (application/json) every
`interval_secs` (default 30; section absent = entirely off).

Wire shape: resource attributes `service.name=ccm`, `service.version`, and
`service.instance.id` = a stable 16-hex FNV-1a hash of `CCM_HOME`
(distinguishes multiple homes pushing to one collector without leaking the
path); counters → monotonic CUMULATIVE integer sums (int64 as decimal
strings per the protobuf JSON mapping); gauges → `asDouble`; the two
histograms → 11 `explicitBounds` in SECONDS and 12 per-bucket DELTA
`bucketCounts`, differenced from PromState's cumulative buckets (whose +Inf
bucket is the histogram `count`). Same honesty rules as the text format:
non-finite cost emits no point, EWMA only after the first sample, and count
traffic stays invisible to every family (the M3 boundary — an SRE dashboard
under-counts requests relative to upstream bills).

Failure isolation: CUMULATIVE temporality means every interval re-sends the
full state — a failed POST is not retried (nothing is lost) and failures
throttle-warn to stderr. The push client is dedicated and timeout-bounded
(3s connect / 5s total, the control-CLI precedent) so a stuck collector
never accumulates stuck tasks. Push failure can never affect routing: the
loop only reads state through `assemble_snapshot`. Credentials boundary
(invariant 1): the payload carries metric names, label values, and counts
only; collector auth headers, if ever needed, come from the
`OTEL_EXPORTER_OTLP_HEADERS` env var ONLY — never config.toml, never
logged, never in the payload. Metrics-only by design — traces stay out
(they would need new JSONL fields and duplicate what `decisions.jsonl`
already persists).

Families:

```text
ccm_up                                   gauge      1 while serving
ccm_process_start_time_seconds           gauge      pinned at serve()
ccm_proxy_requests_total{target,outcome} counter    outcome: success = accepted 2xx
ccm_attempts_total{model,outcome}        counter    5 disjoint outcomes; sum = attempts once every counted attempt has settled (in-flight or aborted header-waits exist only in /_ccm/metrics)
ccm_header_latency_seconds{model}        histogram  fixed 5ms..10s buckets
ccm_latency_ewma_ms{model}               gauge      the in-memory EWMA track
ccm_decision_duration_seconds{target}    histogram  arrival → terminal verdict
ccm_circuit_open{model}                  gauge      1 while the breaker skips the model
ccm_circuit_consecutive_failures{model}  gauge
ccm_tokens_total{model,kind}             counter    kind: input|output|cache_read|cache_write
ccm_cost_micro_usd_total{model}          counter    integer micro-USD at export
ccm_history_dropped_total                counter    dropped history records
```

Latency is deliberately dual-track: the fixed-bucket histogram lets a
Prometheus server compute `histogram_quantile` over any scrape window
(including across proxy restarts — the buckets are cumulative at each
scrape), while the EWMA gauge stays for a glance without a server.

`ccm_attempts_total` is DERIVED at render time from the same per-model
metrics map `/_ccm/metrics` serves (429 counts once, under `rate_limited`),
so the two surfaces cannot disagree. Honest boundaries, same as the
`/_ccm/cost` view: token/cost counters cover only accepted 2xx responses,
prices are hand-entered, cost is proxy-side measurement (not bill truth),
and all runtime counters reset on proxy restart — history queries read disk.

## 9. Routing Decision Trace

Routing Decision Trace is the final major v0.3 routing feature.

One decision is created per proxied request and contains:

- decision id
- timestamp
- runtime target
- selection strategy
- configured candidate order
- ranked candidate order
- score snapshot for every candidate
- circuit decision for each attempted/skipped candidate
- attempt result
- fallback flag
- final selected model
- final outcome

CCM keeps the latest 100 decisions in memory.

Purpose:

> A user or Agent must be able to explain why CCM selected, skipped, retried, or fell back for a specific request.

Persistence shipped in v0.4 M5: every routing decision appends to
`decisions.jsonl` with disk-backed filtered queries (see §13/§18.4); the
in-memory ring (last 100) stays the default view, and full replay tooling
remains unscoped (§17).

## 10. Configuration and State

Default locations:

```text
~/.ccm/config.toml
~/.ccm/state.toml
```

For deterministic test/headless environments:

```text
CCM_HOME=/custom/path
```

Legacy configs with `current` inside `config.toml` are migrated to `state.toml`.

Persisted and runtime selection semantics:

```text
ccm use <target>
    -> writes state.toml

ccm switch <target>
    -> changes running Proxy only
```

## 11. Credentials

Credential resolution order:

```text
CCM_<PROVIDER>_API_KEY
        |
        v
native OS keyring
```

Provider name normalization:

```text
z-ai.test
-> CCM_Z_AI_TEST_API_KEY
```

Configured native backends:

- Windows: Windows Credential Manager
- macOS: Apple Keychain
- Linux: Linux keyutils + Secret Service persistent backend

Environment credential support is required for CI/headless use.

## 12. Claude Code Integration

Direct mode launches `claude` with:

- `ANTHROPIC_BASE_URL`
- `ANTHROPIC_MODEL`
- exactly one of:
  - `ANTHROPIC_API_KEY`
  - `ANTHROPIC_AUTH_TOKEN`

Proxy mode launches Claude against CCM with the placeholder credential
`ANTHROPIC_AUTH_TOKEN=ccm-local-<client-id>` and also exports:

```text
CCM_PROXY_URL
CCM_CLIENT_ID
ANTHROPIC_CUSTOM_HEADERS
```

`CCM_PROXY_URL` allows the in-session `/switch` Skill to target the same
Proxy URL, including non-default ports. The client id identifies the session
to the proxy through two equivalent launcher-injected channels (v0.4): the
`x-ccm-client` line in `ANTHROPIC_CUSTOM_HEADERS` (primary; requires
Claude Code ≥ 2.1.227) and the `ccm-local-<id>` placeholder token (fallback,
version-independent). `CCM_CLIENT_ID` makes `ccm switch` inside the session
default to that client, so `/switch` stays session-scoped. If the parent
environment already sets `ANTHROPIC_CUSTOM_HEADERS`, every existing line is
preserved except `x-ccm-client` lines, which are replaced by exactly one
line for this launch (the proxy reads the first header value; duplicate ids
would resolve unpredictably).

Personal Skill installation path:

```text
~/.claude/skills/switch/SKILL.md
```

Legacy:

```text
~/.claude/commands/switch.md
```

is removed during installation.

## 13. Control API

Current endpoints:

```text
GET  /health
GET  /metrics
GET  /_ccm/status
GET  /_ccm/models
GET  /_ccm/routes
GET  /_ccm/traces
GET  /_ccm/circuits
GET  /_ccm/metrics
GET  /_ccm/scores
GET  /_ccm/decisions
GET  /_ccm/usage
GET  /_ccm/cost
GET  /_ccm/clients
POST /_ccm/switch/{target}
```

`GET /metrics` (v0.4 M7) serves the Prometheus text exposition
(`text/plain; version=0.0.4`) on this same listener — the loopback-only
bind guard covers it; see §8 for the families. `[observability]
prometheus_enabled = false` unregisters the route (404). The OTLP push
loop (v0.5 M4, §8) shares the same exporter state but adds no listener —
it is outbound-only and gated on `[observability.otlp]`, not on this route.

The data-plane forward routes share the listener: `POST /v1/messages`
(the full routing pipeline) and `POST /v1/messages/count_tokens`
(v0.5 M3). The count forward is deliberately primitive: it resolves the
effective target side-effect-free (no counter/last_seen bump), forwards
to the PRIMARY model's upstream only — single attempt, no fallbacks, no
circuit/metric/decision/trace/usage effects (count traffic is invisible
to every metric family) — with a dedicated 10s timeout and
status+body passthrough. An openai-compatible primary has no counting
endpoint: 404 with an Anthropic `not_found_error` envelope, never a
local estimate. A stub gateway answering `input_tokens:0` to a large
request (>8 KiB) passes through unchanged with a one-time process
warning. `/v1/models` and every other auxiliary endpoint stay
unforwarded (M0: the client never calls `/v1/models`; 404s are
tolerated).

Client scoping (v0.4; persistence added v0.5 M2): `POST
/_ccm/switch/{target}?client=<id>` switches only that client's target
(invalid id charset → 400; charset `[A-Za-z0-9._-]{1,64}`), `GET
/_ccm/clients` lists the per-client runtime entries (sorted by client id),
and `GET /_ccm/status|/_ccm/traces|/_ccm/decisions` accept `?client=<id>`
to filter to one client. Scoped entries persist best-effort to
`$CCM_HOME/clients.toml` (§19 note) so they survive restarts; the
`requests` counter is runtime-only and resets on restart — `usage.jsonl`
is the durable ledger.

History queries (v0.4 M5): `GET /_ccm/decisions?since=&until=&model=`
(unix-ms, inclusive) switches the endpoint from the in-memory ring to a
disk read over the persisted history (oldest first). That read is bounded,
not just the response: files are walked newest-first and parsing stops once
`?limit=` matches are held — default 1000 when absent — so the cost scales
with the limit rather than the retained history. `?client=` still applies
on that branch, and a history filter with no running history store is a 400
naming the likely causes. Without any of since/until/model the endpoint
serves the v0.3 in-memory ring; an explicit `?limit=` alone truncates that
view to the newest N.

Control API is currently unauthenticated and intended for localhost use.

### Security note

Binding Proxy to a non-loopback address would expose:

- routing control endpoints
- upstream proxy capability
- access to credentials resolved by CCM

v0.3 enforces loopback-only binding: `ccm proxy` refuses non-loopback bind addresses before loading configuration or opening the listener. Remote/LAN binding requires an authentication design first and remains deferred (open backlog).

## 14. Main Source Modules

```text
src/main.rs        CLI orchestration
src/cli.rs         clap definitions
src/config.rs      declarative configuration
src/state.rs       persisted mutable state
src/clients_store.rs clients.toml persistence for scoped client sessions (v0.5 M2)
src/provider.rs    provider + auth strategy + kind->endpoint mapping
src/model.rs       model/profile definitions
src/route.rs       route + policy definitions
src/manage.rs      add/config commands
src/credential.rs  env/keyring credential resolution
src/launcher.rs    Claude Code process environment
src/integrate.rs   Claude Skill installation
src/health.rs      authenticated provider check
src/discover.rs    /v1/models listing + selective registration (v0.5 M5)
src/history.rs     JSONL observability history engine (writer + readers)
src/history_cli.rs `ccm history` offline presentation
src/usage.rs       usage scanner + UsageRecord + cost aggregation (v0.4 M6)
src/date.rs        UTC calendar-day math for the cost views (Hinnant)
src/advise.rs      `ccm advise` realized-spend cost_weight analysis, never writes config (v0.5 M1)
src/prometheus.rs  hand-rendered text-format exporter: counters + render (v0.4 M7)
src/otlp.rs        hand-rolled OTLP/HTTP JSON push: render_otel + push_once (v0.5 M4)
src/doctor.rs      local environment diagnosis
src/translate.rs   pure anthropic<->openai translation engine (no IO)
src/proxy.rs       forward() + HTTP path + mock integration tests
src/routing/       mod.rs + select.rs / circuit.rs / metrics.rs / decision.rs
src/control/       mod.rs (runtime switch/clients client) + api.rs (control-plane Router + handlers)
```

The v0.4 M0 split moved routing, metrics, circuit breaking, decisions, and
the control API out of `proxy.rs` into `src/routing/` and `src/control/`;
`proxy.rs` keeps the forwarding loop and the HTTP path (see §18.1).

## 15. Tests

Existing unit tests cover core helpers and defaults.

The main v0.3 routing contract has an in-process localhost Mock Provider integration test:

```text
mock_provider_integration_covers_v03_routing_contract
```

Coverage includes:

- 200 SSE-style response
- x-api-key authentication
- bearer authentication
- model rewrite
- 429 fallback
- timeout fallback
- 503 circuit open
- OPEN skip without upstream request
- HALF_OPEN recovery
- weighted routing
- Routing Decision Trace

The openai-compatible translation contract has its own Mock Provider
integration test (v0.4):

```text
mock_openai_provider_integration_covers_translation_contract
```

Coverage includes the translated upstream body and headers, streaming
translation end to end, mixed-protocol 429 fallback, terminal error body
translation, committed-stream failure (error event, no mid-stream
failover), and HALF_OPEN probe release on translate failure.

## 16. Cross-platform Release Target

Target platforms and v0.5 claim status:

```text
Windows  x86_64-pc-windows-msvc     verified 2026-10-10
Linux    x86_64-unknown-linux-gnu   verified 2026-10-10 (WSL2 Ubuntu 24.04)
macOS    aarch64-apple-darwin       not claimed for v0.5 (no host to verify)
macOS    x86_64-apple-darwin        not claimed for v0.5 (no host to verify)
```

See [docs/V0.3_PLAN.md](V0.3_PLAN.md) §3 for the decision record and macOS re-entry criteria.

Repository verification scripts:

```text
scripts/verify.sh
scripts/verify.ps1
scripts/smoke.sh
scripts/smoke.ps1
```

Rust toolchain:

```text
stable
rustfmt
clippy
```

## 17. v0.3 Non-goals

v0.3 shipped without these; the list records what was intentionally out of
scope then and where each item now stands:

- additional selection strategies (still out of scope)
- ~~OpenAI protocol translation~~ — delivered in v0.4 as the
  `openai-compatible` provider kind (`src/translate.rs` + proxy wiring);
  see §4 and the v0.4 plan
- Codex/Aider/OpenCode native integrations (v0.4+ scope)
- ~~persistent metrics database~~ — delivered in v0.4 M5 as append-only
  JSONL metric snapshots (`metrics.jsonl`, 30s cadence, rotation +
  retention) read back by `ccm history metrics` and the history engine;
  deliberately not a queryable database
- ~~persistent decision trace/replay store~~ — delivered in v0.4 M5:
  every routing decision appends to `decisions.jsonl` with disk-backed
  filtered queries (`/_ccm/decisions?since=&until=&model=`,
  `ccm history decisions`); the attempt-trace ring (last 100) stays
  runtime-only by design, and full replay tooling remains unscoped
- ~~Prometheus exporter~~ — delivered in v0.4 M7 (`GET /metrics`
  hand-rendered text format on the same listener; see §8)
- Web UI (still out of scope)
- distributed CCM (still out of scope)
- remote CCM control plane (still out of scope)
- adaptive/self-learning routing (still out of scope)
- real-time token billing (still out of scope)
- ~~provider discovery~~ — delivered in v0.5 M5 as `ccm discover`
  (`GET /v1/models` listing + selective registration, skip-not-overwrite;
  see §4 and `src/discover.rs`)
- mid-stream failover (permanent non-goal, invariant 5)

## 18. Known Design Debt

### 18.1 Proxy file size

Resolved in v0.4 M0: the routing stack now lives in `src/routing/`
(`select` / `circuit` / `metrics` / `decision`) and the control API in
`src/control/api.rs`; `src/proxy.rs` keeps the forwarding loop and the
HTTP path. It is still the largest module — prefer extracting a new
concern over growing it further.

### 18.2 Policy update UX

`ccm add route` currently uses default values for policy flags.

Re-running the command on an existing Route may reset unspecified policy values.

Post-v0.3 options:

- `ccm route policy set`
- make update flags optional and preserve existing values

### 18.3 HTTP fallback validation

`fallback_on` is parsed as `u16`.

Validation should eventually enforce legal HTTP ranges and possibly deduplicate values.

### 18.4 Runtime observability persistence

Resolved across v0.4 M5–M7. M5: routing decisions, periodic metric
snapshots, and circuit transitions persist to append-only JSONL under
`$CCM_HOME/history/` (rotation + 14-day retention, single-writer OS lock;
query via `/_ccm/decisions?since=&until=&model=` or `ccm history`, both
offline-capable for the CLI). Runtime metrics and circuit state still
restart from zero — deliberate, so stale history never distorts
healthiest/weighted ordering — and the attempt-trace ring stays
in-memory-only (last 100) by design.

v0.4 M6 extends the same store with `usage.jsonl`: one usage record per
accepted 2xx response (decision-id joined, client tagged, priced against
a snapshot of the hand-entered `[models.<name>.pricing]` table; unpriced
models record `cost_usd: null`, never a guess). Queries: `/_ccm/usage`
(same dual-branch parameter contract as decisions), `/_ccm/cost?day=`
(UTC-day aggregation, shared core with `ccm history cost`).

v0.4 M7 adds the export side: `GET /metrics` on the same listener (see
§8) hands the runtime counters to a monitoring system, which closes the
"everything dies with the proxy" gap for live signals too — histograms
are cumulative per scrape, so a Prometheus server sees across restarts
even though the proxy's own counters reset.

### 18.5 Query strings

Proxy forwarding covers `/v1/messages` and, since v0.5 M3,
`/v1/messages/count_tokens` (primary-only, side-effect-free; see §13).
Neither carries query-string semantics.

If future Anthropic-compatible APIs depend on query strings, forwarding behavior must be reviewed.

### 18.6 Non-stream translation buffers the upstream body

For non-streaming openai-compatible responses, the whole upstream body is
read inside `forward()` before the client receives any byte, because the
translation needs the complete JSON. `header_timeout_ms` only bounds time
to upstream response headers, so a stalled committed body holds the client
request open. Accepted for v0.4 — this mirrors reqwest's default of no
body-level timeout; revisit only if it bites in practice.

## 19. Design Invariants for Future Agents

Do not change these without an explicit design decision:

1. Route names must remain visible as runtime targets; do not collapse them to primary model aliases.
2. Circuit state is model-scoped.
3. Circuit skip does not consume attempt budget.
4. Selection ranking happens before circuit admission.
5. No mid-stream failover.
6. Runtime metrics must not require buffering SSE.
7. Provider authentication is explicit.
8. `ccm switch` is runtime-only for the GLOBAL target; scoped client
   entries persist best-effort to `clients.toml` (v0.5 rewording — never
   `state.toml`).
9. `ccm use` is persisted state.
10. v0.3 routing feature set is frozen.
11. The proxy refuses non-loopback bind addresses in v0.3.

Note on the openai-compatible translation (v0.4): translation is an
upstream-protocol concern only and does not loosen invariants 5, 6, or 7.
The commit point remains the upstream response headers; after that a
streaming translation failure produces the terminal Anthropic `error`
event and body end, never a mid-stream failover (5). The SSE translator
transforms chunk-by-chunk with a 1 MiB per-frame cap and never buffers the
whole stream (6). Exactly one credential header — resolved per kind — is
injected upstream (7).

Note on multi-client routing (v0.4, amended v0.5 M2): the client id is
identity, not authentication. Any local process can forge `x-ccm-client`
or the `ccm-local-<id>` token; it shares the trust domain of the
loopback-only, unauthenticated control API (§13 security note). Scoped
entries persist best-effort to `$CCM_HOME/clients.toml` (8, reworded):
`id` / `target` / `last_seen_ms` only, revalidated against current config
at startup, TTL 7 days, LRU cap 256, `[clients] persist = false` restores
memory-only. The global switch stays runtime-only, and `ccm use` /
`state.toml` are untouched (9). Two proxies on one `CCM_HOME` are
last-writer-wins on `clients.toml` — the scenario is already degraded for
history (single-writer lock); `clients.toml` takes no lock by design.

Note on usage capture (v0.4 M6): the usage scanner wraps accepted bodies
chunk-by-chunk with a 64 KiB per-line cap and never buffers the stream
(6, same reasoning as runtime metrics). Cost accounting is proxy-side
measurement, not bill truth: token counts come from upstream-reported
usage and prices are hand-entered.
