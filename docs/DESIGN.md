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
```

Current provider kinds:

- `anthropic`
- `anthropic-compatible`

Current authentication styles:

- `x-api-key`
- `bearer`

Important invariant:

> Exactly one upstream credential header is injected by CCM.

CCM strips inbound `x-api-key` and `Authorization` before forwarding and then injects the configured provider credential.

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

No persistence is planned for v0.3.

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

Persistence/replay storage is deferred to post-v0.3.

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

Proxy mode launches Claude against CCM and also exports:

```text
CCM_PROXY_URL
```

This allows the in-session `/switch` Skill to target the same Proxy URL, including non-default ports.

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
GET  /_ccm/status
GET  /_ccm/models
GET  /_ccm/routes
GET  /_ccm/traces
GET  /_ccm/circuits
GET  /_ccm/metrics
GET  /_ccm/scores
GET  /_ccm/decisions
POST /_ccm/switch/{target}
```

Control API is currently unauthenticated and intended for localhost use.

### Security note

Binding Proxy to a non-loopback address would expose:

- routing control endpoints
- upstream proxy capability
- access to credentials resolved by CCM

Before v0.3 release, maintainers should decide whether to enforce loopback-only binding. Remote control authentication is explicitly out of scope for v0.3.

## 14. Main Source Modules

```text
src/main.rs        CLI orchestration
src/cli.rs         clap definitions
src/config.rs      declarative configuration
src/state.rs       persisted mutable state
src/provider.rs    provider + auth strategy
src/model.rs       model/profile definitions
src/route.rs       route + policy definitions
src/manage.rs      add/config commands
src/credential.rs  env/keyring credential resolution
src/launcher.rs    Claude Code process environment
src/integrate.rs   Claude Skill installation
src/control.rs     runtime switch client
src/health.rs      authenticated provider check
src/doctor.rs      local environment diagnosis
src/proxy.rs       routing engine + control plane
```

`src/proxy.rs` currently contains routing, metrics, circuit breaking, decisions, mock integration tests, and HTTP handlers. This is acceptable for v0.3 stabilization, but post-v0.3 refactoring should split these concerns.

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

## 16. Cross-platform Release Target

Target platforms:

```text
Windows  x86_64-pc-windows-msvc
Linux    x86_64-unknown-linux-gnu
macOS    aarch64-apple-darwin
macOS    x86_64-apple-darwin
```

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

Do not add these before v0.3 release:

- additional selection strategies
- OpenAI protocol translation
- Codex/Aider/OpenCode native integrations
- persistent metrics database
- persistent decision trace/replay store
- Prometheus exporter
- Web UI
- distributed CCM
- remote CCM control plane
- adaptive/self-learning routing
- real-time token billing
- provider discovery
- mid-stream failover

## 18. Known Design Debt

### 18.1 Proxy file size

`src/proxy.rs` is large and should be split after v0.3.

Suggested future modules:

```text
proxy/http.rs
routing/select.rs
routing/circuit.rs
routing/metrics.rs
routing/decision.rs
control/api.rs
```

Do not perform this refactor before release unless required by a correctness problem.

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

Metrics, traces, circuits, and decisions reset with Proxy restart.

This is intentional for v0.3.

### 18.5 Query strings

Proxy forwarding currently focuses on `/v1/messages`.

If future Anthropic-compatible APIs depend on query strings, forwarding behavior must be reviewed.

## 19. Design Invariants for Future Agents

Do not change these without an explicit design decision:

1. Route names must remain visible as runtime targets; do not collapse them to primary model aliases.
2. Circuit state is model-scoped.
3. Circuit skip does not consume attempt budget.
4. Selection ranking happens before circuit admission.
5. No mid-stream failover.
6. Runtime metrics must not require buffering SSE.
7. Provider authentication is explicit.
8. `ccm switch` is runtime-only.
9. `ccm use` is persisted state.
10. v0.3 routing feature set is frozen.
