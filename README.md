# CCM

A fast local model manager and control plane for AI coding CLIs, starting with Claude Code.

CCM manages providers, models, profiles, routes, route policies, runtime switching, fallback execution, circuit breaking, metrics, and dynamic model selection locally.

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

## Configuration and state

CCM separates declarative configuration from mutable local state:

```text
~/.ccm/config.toml
  providers
  models
  profiles
  routes
  route policies

~/.ccm/state.toml
  current
```

A fresh `ccm init` creates both files. Example state:

```toml
current = "claude"
```

`ccm use <target>` writes only `state.toml`. Runtime `ccm switch <target>` remains in-memory and does not modify either file.

Older CCM configurations that stored:

```toml
current = "glm"
```

inside `config.toml` are migrated automatically the first time CCM loads persisted state. CCM writes that value to `state.toml` and rewrites `config.toml` without the legacy `current` field.

## Provider authentication

Provider authentication is explicit and only one upstream auth header is emitted:

```toml
[providers.anthropic]
kind = "anthropic"
base_url = "https://api.anthropic.com"
auth = "x-api-key"

[providers.gateway]
kind = "anthropic-compatible"
base_url = "https://gateway.example.com"
auth = "bearer"
```

Supported values:

```text
x-api-key
bearer
```

If an older provider entry omits `auth`, CCM defaults to `x-api-key`.

For direct Claude Code launches, CCM also isolates the environment: `x-api-key` sets only `ANTHROPIC_API_KEY`, while `bearer` sets only `ANTHROPIC_AUTH_TOKEN`. This avoids accidentally inheriting both authentication variables from the parent shell.

Create a provider explicitly with:

```bash
ccm add provider gateway \
  --kind anthropic-compatible \
  --base-url https://gateway.example.com \
  --auth bearer
```

## Model routing metadata

Models can declare static metadata used by cost-aware and weighted routing:

```toml
[models.claude]
provider = "anthropic"
model_id = "claude-sonnet-5-5"

[models.claude.routing]
cost_weight = 1.0
quality_weight = 1.0

[models.glm]
provider = "zai"
model_id = "glm-5.3"

[models.glm.routing]
cost_weight = 0.25
quality_weight = 0.85
```

`cost_weight` is a relative cost factor where lower is cheaper. `quality_weight` is a user-supplied relative quality signal; weighted routing clamps it to the 0..1 range. These are routing hints, not measured billing or benchmark data.

Create models from CLI with:

```bash
ccm add model glm \
  --provider zai \
  --model-id glm-5.3 \
  --cost-weight 0.25 \
  --quality-weight 0.85
```

Existing model configs that omit `[models.<name>.routing]` remain valid and default both values to `1.0`.

## Route model

```toml
[routes.coding-route]
primary = "claude"
fallback = ["glm", "kimi"]

[routes.coding-route.policy]
selection = "weighted"
header_timeout_ms = 30000
fallback_on = [429, 502, 503, 504]
max_attempts = 3
backoff_ms = 200

[routes.coding-route.policy.weights]
reliability = 0.4
latency = 0.2
cost = 0.2
quality = 0.2

[routes.coding-route.policy.circuit_breaker]
enabled = true
failure_threshold = 3
open_ms = 30000
```

The routing stack is:

```text
Model metadata + Runtime metrics
              │
              ▼
Route → Candidate Selector
              │
              ▼
       Circuit Breaker
              │
              ▼
       Policy Executor
              │
              ▼
            Proxy
```

## Selection strategies

`RoutePolicy.selection` supports:

```text
ordered
healthiest
lowest-latency
lowest-cost
weighted
```

### ordered

Preserves the configured candidate order:

```text
primary → fallback[0] → fallback[1]
```

This is the default.

### healthiest

Ranks candidates by observed success rate after at least three real upstream attempts. Candidates with fewer than three samples use a neutral reliability rank. Ties prefer lower response-header latency.

### lowest-latency

Ranks candidates by EWMA response-header latency. Candidates without samples remain behind measured candidates.

### lowest-cost

Ranks candidates by ascending model `routing.cost_weight`. This uses static user-configured metadata and does not require runtime samples.

### weighted

Computes an explicit score for every candidate:

```text
reliability_score = success_rate, or 1.0 before 3 samples
latency_score     = 1 / (1 + latency_ewma_ms / 1000)
cost_score        = 1 / (1 + model.cost_weight)
quality_score     = clamp(model.quality_weight, 0, 1)

weighted_score =
  (
    reliability_weight × reliability_score +
    latency_weight     × latency_score +
    cost_weight        × cost_score +
    quality_weight     × quality_score
  )
  /
  (
    reliability_weight +
    latency_weight +
    cost_weight +
    quality_weight
  )
```

Default route weights are:

```toml
[routes.example.policy.weights]
reliability = 0.4
latency = 0.2
cost = 0.2
quality = 0.2
```

All weights must be non-negative. Weighted selection requires at least one positive weight.

Circuit Breaker admission always runs after candidate sorting. A model with a high score is still skipped when its circuit is OPEN or a HALF_OPEN probe is already in flight.

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

`health_score` is currently:

```text
health_score = success_rate × 100
```

`latency_ewma_ms` measures time to upstream response headers so CCM does not buffer streamed responses. EWMA alpha is `0.2`.

Inspect metrics:

```bash
curl http://127.0.0.1:13521/_ccm/metrics
```

## Fallback policy

Each route can configure:

```toml
header_timeout_ms = 30000
fallback_on = [429, 502, 503, 504]
max_attempts = 3
backoff_ms = 200
```

Connection/request errors and response-header timeouts are fallback-eligible. `max_attempts` counts real upstream attempts; circuit-skipped models do not consume the budget.

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

Health failures are connection/request errors, response-header timeouts, and HTTP statuses listed in `fallback_on`. Authentication/request errors such as 400/401/403 are not health failures unless explicitly configured.

Circuit state is keyed by model alias and shared across routes inside one running proxy.

## Creating dynamic routes from CLI

Reliability-focused:

```bash
ccm add route coding \
  --primary claude \
  --fallback glm,kimi \
  --selection healthiest
```

Latency-focused:

```bash
ccm add route fast \
  --primary glm \
  --fallback claude,kimi \
  --selection lowest-latency
```

Cost-focused:

```bash
ccm add route cheap \
  --primary claude \
  --fallback glm,kimi \
  --selection lowest-cost
```

Balanced weighted routing:

```bash
ccm add route balanced \
  --primary claude \
  --fallback glm,kimi \
  --selection weighted \
  --reliability-weight 0.4 \
  --latency-weight 0.2 \
  --cost-weight 0.2 \
  --quality-weight 0.2
```

## Runtime switching

```bash
ccm switch glm
ccm switch fast
ccm switch balanced
```

`ccm switch` changes only the running proxy's in-memory target. `ccm use` changes the persisted default in `~/.ccm/state.toml`.

## Control API

```text
GET  /health
GET  /_ccm/status
GET  /_ccm/models
GET  /_ccm/routes
GET  /_ccm/traces
GET  /_ccm/circuits
GET  /_ccm/metrics
GET  /_ccm/scores
GET  /_ccm/decisions
POST /_ccm/switch/{model-or-profile-or-route}
```

`/_ccm/models` exposes model cost/quality metadata. `/_ccm/status` and `/_ccm/routes` expose selection strategy, scoring weights, fallback policy, and circuit-breaker policy. `/_ccm/scores` explains the active route's current candidate scores. `/_ccm/decisions` returns the most recent complete per-request routing decisions.

## Routing Decision Trace

Routing Decision Trace is the final major v0.3 routing feature. CCM keeps the most recent 100 decisions in proxy memory.

Inspect them with:

```bash
curl http://127.0.0.1:13521/_ccm/decisions
```

A decision records the state used at request time rather than reconstructing it later:

```json
{
  "id": 42,
  "timestamp_ms": 1790820000000,
  "target": "balanced",
  "selection": "weighted",
  "configured_candidates": ["claude", "glm", "kimi"],
  "ranked_candidates": [
    {
      "rank": 1,
      "model": "glm",
      "reliability_score": 0.96,
      "latency_score": 0.71,
      "cost_score": 0.80,
      "quality_score": 0.85,
      "weighted_score": 0.864
    }
  ],
  "attempts": [
    {
      "attempt": 1,
      "model": "glm",
      "circuit": "CLOSED",
      "result": "HTTP 429 Too Many Requests",
      "fallback": true
    },
    {
      "attempt": 2,
      "model": "claude",
      "circuit": "CLOSED",
      "result": "HTTP 200 OK",
      "fallback": false
    }
  ],
  "selected": "claude",
  "outcome": "HTTP 200 OK"
}
```

This makes one routing request answer four questions directly:

```text
Why was this candidate ranked first?
Was any candidate skipped by the circuit breaker?
Why did CCM fallback?
Which model finally produced the returned upstream response?
```

Decision traces are intentionally in-memory for v0.3. Persistence and replay storage are deferred until after the stabilization release.

## Mock Provider integration coverage

CCM includes an in-process integration-style test backed by real localhost TCP mock providers. It exercises the routing core through real `reqwest` upstream calls and covers:

```text
200 SSE-style streaming response
x-api-key authentication
bearer authentication
model ID rewrite
429 fallback
503 fallback
response-header timeout fallback
Circuit Breaker OPEN skip
HALF_OPEN recovery
weighted candidate selection
Routing Decision Trace
```

The test isolates configuration with `CCM_HOME` and credentials with `CCM_<PROVIDER>_API_KEY`, so it does not depend on a developer's real `~/.ccm` files or OS keyring.

Run it with:

```bash
cargo test mock_provider_integration_covers_v03_routing_contract -- --nocapture
```

## v0.3 stabilization boundary

Routing Decision Trace closes feature development for v0.3. No additional selection strategies are planned before the stabilization release.

The remaining v0.3 work is reliability-focused:

```text
Windows / Linux / macOS validation
re-enable CI and make all checks green
release packaging and documentation cleanup
```

## In-session switching from Claude Code

Install once:

```bash
ccm integrate claude
```

Then inside a Claude Code session started with `ccm run --proxy`:

```text
/switch glm
/switch claude
/switch balanced
```

## Commands

```text
ccm init [--force]
ccm add provider <name> [--base-url URL] [--kind KIND] [--auth x-api-key|bearer]
ccm add model <name> [--provider PROVIDER] [--model-id MODEL]
  [--cost-weight N]
  [--quality-weight N]
ccm add route <name> --primary MODEL [--fallback MODEL1,MODEL2]
  [--selection ordered|healthiest|lowest-latency|lowest-cost|weighted]
  [--reliability-weight N]
  [--latency-weight N]
  [--cost-weight N]
  [--quality-weight N]
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
- static config / persisted state separation via config.toml + state.toml
- automatic migration from legacy config current
- explicit x-api-key / bearer provider authentication
- single-header upstream credential injection
- environment credential fallback for headless/CI use
- isolated CCM_HOME for deterministic test environments
- Claude Code gateway environment compatibility verified against current Anthropic docs
- modern Claude Code personal Skill integration for /switch
- fallback policy and retry budget
- model-level circuit breaker
- runtime reliability and latency metrics
- model cost/quality routing metadata
- `ordered`, `healthiest`, `lowest-latency`, `lowest-cost`, and `weighted` selection
- transparent weighted scoring
- explainable candidate score control API
- complete per-request Routing Decision Trace
- localhost Mock Provider integration coverage for the v0.3 routing contract
- in-memory traces, metrics, decisions, and circuit-state control APIs
- no mid-stream failover
- no OpenAI protocol translation yet

## License

MIT
