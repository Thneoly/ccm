# CCM

A fast local model manager and control plane for AI coding CLIs, starting with Claude Code.

CCM manages providers, models, profiles, routes, route policies, runtime switching, fallback execution, circuit breaking, and metric-aware model selection locally.

## Quick start

```bash
git clone https://github.com/Thneoly/ccm.git
cd ccm
cargo build --release
ccm init
ccm auth set anthropic
ccm auth set zai
ccm proxy
ccm run --proxy
```

## Route model

```toml
[routes.coding-route]
primary = "claude"
fallback = ["glm", "kimi"]

[routes.coding-route.policy]
selection = "healthiest"
header_timeout_ms = 30000
fallback_on = [429, 502, 503, 504]
max_attempts = 3
backoff_ms = 200

[routes.coding-route.policy.circuit_breaker]
enabled = true
failure_threshold = 3
open_ms = 30000
```

The routing stack is:

```text
Provider
   ↓
Model
   ↓
Route
   ↓
RoutePolicy
   ↓
Runtime metrics + Circuit state
   ↓
Metric-aware Routing Executor
   ↓
CCM Proxy
```

## Selection strategies

`RoutePolicy.selection` supports:

```text
ordered
healthiest
lowest-latency
```

### ordered

Preserves the configured candidate order:

```text
primary → fallback[0] → fallback[1]
```

This is the default and preserves existing CCM behavior.

### healthiest

Ranks candidates by observed success rate after at least three real upstream attempts. Candidates with fewer than three samples are treated as neutral so CCM does not aggressively reorder on one noisy request. Ties are broken by lower EWMA response-header latency.

### lowest-latency

Ranks candidates by EWMA response-header latency. Candidates without latency samples remain behind measured candidates; equal/unknown candidates preserve configured order.

Circuit Breaker admission always runs after selection. An OPEN model is skipped even if a metric strategy ranked it first.

## Runtime metrics

CCM keeps per-model metrics in memory for the lifetime of the proxy:

```text
attempts
successes
success_rate
health_score
http_errors
fallback_failures
timeouts
request_errors
rate_limited
latency_ewma_ms
last_success_ms
last_failure_ms
```

`health_score` is currently an intentionally simple and explainable reliability score:

```text
health_score = success_rate × 100
```

`latency_ewma_ms` measures time to upstream response headers, not full streamed completion time, so CCM does not need to buffer SSE responses. EWMA uses alpha = 0.2.

Inspect metrics with:

```bash
curl http://127.0.0.1:13521/_ccm/metrics
```

Example:

```json
[
  {
    "model": "claude",
    "attempts": 12,
    "successes": 9,
    "success_rate": 0.75,
    "health_score": 75.0,
    "http_errors": 3,
    "fallback_failures": 2,
    "timeouts": 0,
    "request_errors": 0,
    "rate_limited": 2,
    "latency_ewma_ms": 842.4,
    "last_success_ms": 1790820000000,
    "last_failure_ms": 1790819900000
  }
]
```

## Fallback policy

Each route can configure:

```toml
header_timeout_ms = 30000
fallback_on = [429, 502, 503, 504]
max_attempts = 3
backoff_ms = 200
```

Connection/request errors and response-header timeouts are fallback-eligible. `max_attempts` counts real upstream attempts; models skipped by an OPEN/HALF_OPEN circuit do not consume attempt budget.

Once an upstream response is accepted, CCM streams it directly back to Claude Code. CCM does not switch models in the middle of an already-started stream.

## Circuit Breaker

```toml
[routes.coding-route.policy.circuit_breaker]
enabled = true
failure_threshold = 3
open_ms = 30000
```

State machine:

```text
CLOSED
   │ failures >= threshold
   ▼
OPEN
   │ cooldown elapsed
   ▼
HALF_OPEN
   ├─ probe succeeds → CLOSED
   └─ probe fails    → OPEN
```

Health failures are connection/request errors, response-header timeouts, and HTTP statuses listed in `fallback_on`. Authentication/request errors such as 400/401/403 are not health failures unless explicitly configured in `fallback_on`.

Circuit state is keyed by model alias and shared across routes inside one running proxy.

Inspect it with:

```bash
curl http://127.0.0.1:13521/_ccm/circuits
```

## Creating routes from CLI

```bash
ccm add route coding \
  --primary claude \
  --fallback glm,kimi \
  --selection healthiest \
  --header-timeout-ms 30000 \
  --fallback-on 429,502,503,504 \
  --max-attempts 3 \
  --backoff-ms 200 \
  --circuit-enabled true \
  --failure-threshold 3 \
  --circuit-open-ms 30000
```

For latency-first routing:

```bash
ccm add route fast \
  --primary glm \
  --fallback claude,kimi \
  --selection lowest-latency
```

## Runtime switching

```bash
ccm switch glm
ccm switch fast
ccm switch coding-route
```

`ccm switch` changes only the running proxy's in-memory target. `ccm use` changes the persisted default in `~/.ccm/config.toml`.

## Control API

```text
GET  /health
GET  /_ccm/status
GET  /_ccm/models
GET  /_ccm/routes
GET  /_ccm/traces
GET  /_ccm/circuits
GET  /_ccm/metrics
POST /_ccm/switch/{model-or-profile-or-route}
```

The status and route endpoints expose the effective selection, fallback, and circuit-breaker policy.

## In-session switching from Claude Code

Install once:

```bash
ccm integrate claude
```

Then inside a Claude Code session started with `ccm run --proxy`:

```text
/switch glm
/switch claude
/switch coding-route
```

## Commands

```text
ccm init [--force]
ccm add provider <name> [--base-url URL] [--kind KIND]
ccm add model <name> [--provider PROVIDER] [--model-id MODEL]
ccm add route <name> --primary MODEL [--fallback MODEL1,MODEL2]
  [--selection ordered|healthiest|lowest-latency]
  [--header-timeout-ms MS]
  [--fallback-on CODE1,CODE2]
  [--max-attempts N]
  [--backoff-ms MS]
  [--circuit-enabled true|false]
  [--failure-threshold N]
  [--circuit-open-ms MS]
ccm integrate claude [--remove]
ccm doctor
ccm proxy [--bind HOST:PORT]
ccm list
ccm current
ccm use <model-or-profile-or-route>
ccm switch <model-or-profile-or-route> [--proxy-url URL]
ccm run [model-or-profile] [--proxy] [--proxy-url URL]
ccm auth set <provider>
ccm auth delete <provider>
ccm health <model-or-profile>
```

## Development

```bash
cargo fmt --all -- --check
cargo check --all-targets
cargo test --all-targets
cargo clippy --all-targets -- -D warnings
```

GitHub Actions CI is currently disabled.

## Current scope

- Anthropic-compatible `/v1/messages`
- streaming upstream responses
- model/profile/route runtime switching
- route-specific fallback policy
- timeout/backoff/max attempts
- model-level circuit breaker
- CLOSED / OPEN / HALF_OPEN behavior
- per-model runtime metrics
- success-rate health score
- latency EWMA
- `ordered`, `healthiest`, and `lowest-latency` selection
- in-memory traces, metrics, and circuit-state control APIs
- no mid-stream failover
- no cost-aware/weighted routing yet
- no OpenAI protocol translation yet

## License

MIT
