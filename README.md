# CCM

A fast local model manager and control plane for AI coding CLIs, starting with Claude Code.

CCM manages providers, models, profiles, routes, route policies, runtime switching, fallback execution, and model health state locally.

## Quick start

```bash
git clone https://github.com/Thneoly/ccm.git
cd ccm
cargo build --release
ccm init
```

Store credentials and start the proxy:

```bash
ccm auth set anthropic
ccm auth set zai
ccm proxy
```

Then launch Claude Code through CCM:

```bash
ccm run --proxy
```

## Runtime switching

```bash
ccm switch glm
ccm switch fast
ccm switch coding-route
```

`ccm switch` changes only the running proxy's in-memory target. `ccm use` changes the persisted default in `~/.ccm/config.toml`.

## Route model

```toml
[routes.coding-route]
primary = "claude"
fallback = ["glm", "kimi"]

[routes.coding-route.policy]
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
Runtime Target
   ↓
Health-aware Routing Executor
   ↓
CCM Proxy
```

## Route Policy

Each route can configure:

- `header_timeout_ms`: maximum wait for upstream response headers per attempt.
- `fallback_on`: HTTP status codes that trigger fallback.
- `max_attempts`: total real upstream attempts, including the primary.
- `backoff_ms`: delay before trying the next candidate.

If omitted, the defaults are:

```toml
header_timeout_ms = 30000
fallback_on = [429, 502, 503, 504]
max_attempts = 3
backoff_ms = 200
```

Connection/request errors and response-header timeouts are also fallback-eligible when another candidate is available.

Once an upstream response is accepted, CCM streams it directly back to Claude Code. CCM does not switch models in the middle of an already-started SSE stream.

## Health-aware Routing / Circuit Breaker

Each route also has a circuit breaker policy:

```toml
[routes.coding-route.policy.circuit_breaker]
enabled = true
failure_threshold = 3
open_ms = 30000
```

Defaults:

```text
enabled = true
failure_threshold = 3
open_ms = 30000
```

The model circuit states are:

```text
CLOSED
   │
   │ consecutive health failures >= failure_threshold
   ▼
OPEN
   │
   │ open_ms elapsed
   ▼
HALF_OPEN
   │
   ├─ probe succeeds ─────────────→ CLOSED
   │
   └─ probe fails ────────────────→ OPEN
```

Health failures are:

```text
connection/request error
response-header timeout
HTTP status listed in fallback_on
```

Responses such as `400`, `401`, and `403` are not counted as model-health failures unless you explicitly add them to `fallback_on`. This prevents authentication or request-configuration problems from incorrectly marking a model as unhealthy.

An OPEN model is skipped before an upstream request is sent and does not consume `max_attempts`. After the cooling period expires, one request is allowed through as a HALF_OPEN probe. Concurrent requests skip that model while the probe is in flight.

Because circuit state is keyed by model alias, the same model's health state is shared across routes inside one running CCM proxy.

## Creating a policy-aware route

```bash
ccm add route coding \
  --primary claude \
  --fallback glm,kimi \
  --header-timeout-ms 30000 \
  --fallback-on 429,502,503,504 \
  --max-attempts 3 \
  --backoff-ms 200 \
  --circuit-enabled true \
  --failure-threshold 3 \
  --circuit-open-ms 30000
```

Disable circuit breaking for one route with:

```bash
ccm add route experimental \
  --primary claude \
  --fallback glm \
  --circuit-enabled false
```

## Attempt tracing

Example stderr output:

```text
ccm route=coding attempt=1 model=claude result=HTTP 429 Too Many Requests action=fallback
ccm route=coding attempt=2 model=glm result=HTTP 200 OK
```

When a circuit is open:

```text
ccm route=coding attempt=1 model=claude result=skipped: circuit OPEN until ... action=fallback
ccm route=coding attempt=1 model=glm result=HTTP 200 OK
```

The proxy keeps the most recent 100 attempt traces in memory.

## Control API

```text
GET  /health
GET  /_ccm/status
GET  /_ccm/models
GET  /_ccm/routes
GET  /_ccm/traces
GET  /_ccm/circuits
POST /_ccm/switch/{model-or-profile-or-route}
```

Examples:

```bash
curl http://127.0.0.1:13521/_ccm/status
curl http://127.0.0.1:13521/_ccm/routes
curl http://127.0.0.1:13521/_ccm/traces
curl http://127.0.0.1:13521/_ccm/circuits
```

Example circuit view:

```json
[
  {
    "model": "claude",
    "state": "OPEN",
    "consecutive_failures": 3,
    "open_until_ms": 1790820030000
  },
  {
    "model": "glm",
    "state": "CLOSED",
    "consecutive_failures": 0,
    "open_until_ms": null
  }
]
```

`/_ccm/status` and `/_ccm/routes` also expose the effective route policy, including circuit-breaker settings.

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
- configurable timeout/backoff/max attempts
- model-level circuit breaker
- CLOSED / OPEN / HALF_OPEN behavior
- in-memory attempt traces
- circuit-state control API
- no mid-stream failover
- no OpenAI protocol translation yet

## License

MIT
