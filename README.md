# CCM

A fast local model manager and control plane for AI coding CLIs, starting with Claude Code.

CCM manages providers, models, profiles, routes, route policies, runtime switching, fallback execution, circuit breaking, metrics, and dynamic model selection locally.

User guide (中文): [docs/USAGE.md](docs/USAGE.md).

## Agent handoff

For implementation continuation and release stabilization, read:

- [docs/DESIGN.md](docs/DESIGN.md) — architecture, routing semantics, invariants, module responsibilities, known design debt.
- [docs/V0.5_PLAN.md](docs/V0.5_PLAN.md) — v0.5 scope, milestones, landing records, and the post-v0.5 backlog (v0.5.0 released 2026-10-10).
- [docs/V0.4_PLAN.md](docs/V0.4_PLAN.md) — v0.4 scope, milestones, landing records, and the post-v0.4 backlog (v0.4.0 released 2026-10-03).
- [docs/V0.3_PLAN.md](docs/V0.3_PLAN.md) — v0.3 verified/unverified status, Release Gate, platform checklist, security decision, handoff order.
- [docs/USAGE.md](docs/USAGE.md) — task-oriented user guide (Chinese).

Current status:

> **v0.5.0 released (2026-10-10)** — [GitHub Release](https://github.com/Thneoly/ccm/releases/tag/v0.5.0). v0.5 adds `ccm advise` (cost_weight suggestions from realized spend), persistent client sessions (`clients.toml`), `count_tokens` forwarding, OTLP/HTTP JSON metrics export, and provider discovery (`ccm discover`). v0.4.0: [GitHub Release](https://github.com/Thneoly/ccm/releases/tag/v0.4.0). v0.3.0: [GitHub Release](https://github.com/Thneoly/ccm/releases/tag/v0.3.0).

Post-v0.5 work follows the backlog in `docs/V0.5_PLAN.md` §11; keep the CI release gate green on every push to main.

## Install

Download a prebuilt binary from the [GitHub Releases](https://github.com/Thneoly/ccm/releases/latest) page (v0.4.0 and later): Windows x64 `ccm-v<ver>-x86_64-pc-windows-msvc.exe` and Linux x64 `ccm-v<ver>-x86_64-unknown-linux-gnu` (built on Ubuntu 24.04, needs glibc ≥ 2.39), with SHA-256 hashes in `checksums.txt`. Drop the binary into a directory on your PATH.

Or build the release binary from source and install it onto your PATH:

```powershell
# Windows PowerShell
./scripts/install.ps1
```

```bash
# Linux / macOS
./scripts/install.sh
```

Default install locations:

```text
Windows  %LOCALAPPDATA%\Programs\ccm   (user PATH, no admin required)
Linux    ~/.local/bin
macOS    ~/.local/bin
```

Override the destination with `-Destination <dir>` (PowerShell) or `CCM_INSTALL_DIR=<dir>` (sh). Re-running the script upgrades an existing installation in place.

macOS is unverified for v0.5 — there is no macOS host to run the release gate on. See [Supported release platforms](#supported-release-platforms).

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

[providers.deepseek]
kind = "openai-compatible"
base_url = "https://api.deepseek.com"
```

Supported values:

```text
x-api-key
bearer
```

If a provider entry omits `auth`, the default is resolved by kind: `x-api-key` for `anthropic` and `anthropic-compatible` (unchanged), `bearer` for `openai-compatible`. An explicit `auth` always wins.

For direct Claude Code launches, CCM also isolates the environment: `x-api-key` sets only `ANTHROPIC_API_KEY`, while `bearer` sets only `ANTHROPIC_AUTH_TOKEN`. This avoids accidentally inheriting both authentication variables from the parent shell.

Create a provider explicitly with:

```bash
ccm add provider gateway \
  --kind anthropic-compatible \
  --base-url https://gateway.example.com \
  --auth bearer

ccm add provider deepseek \
  --kind openai-compatible \
  --base-url https://api.deepseek.com
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

Instead of hand-copying `model_id`s from a gateway console, `ccm discover <provider>` (v0.5) lists the gateway's `GET /v1/models` and registers the ids you select — already-registered `(provider, model_id)` pairs are skipped, never overwritten, and each unpriced selection prints a commented `[models.<alias>.pricing]` skeleton to hand-enter prices (see the user guide §3.5).

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

The same runtime is exported in the Prometheus text format at
`GET /metrics` (v0.4) — on the proxy's single loopback-only listener, no
second port. Families cover request/attempt outcomes (by target and model),
a fixed-bucket header-latency histogram alongside the EWMA gauge,
decision duration, circuit state, tokens, and cost in integer micro-USD.
`[observability] prometheus_enabled = false` removes the route entirely.

```bash
curl -s http://127.0.0.1:13521/metrics | grep ccm_
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

While the OPEN cooldown has elapsed but the next HALF_OPEN probe has not been admitted yet, `/_ccm/circuits` reports the model as `HALF_OPEN_READY`.

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

Multiple terminals can hold different targets on one proxy (v0.4): each `ccm run --proxy` session carries a client id, and `ccm switch <target> --client <id>` switches only that client. `--global` moves the global target that un-switched clients follow; a bare `ccm switch` is global only outside a `ccm run --proxy` session — inside one it inherits the session's `CCM_CLIENT_ID` and stays client-scoped. `ccm clients` lists the per-client targets. Since v0.5, scoped client targets persist best-effort to `$CCM_HOME/clients.toml` and survive proxy restarts (revalidated against current config at startup, TTL 7 days, LRU cap 256; the per-client request counter resets — `usage.jsonl` is the durable ledger).

## Control API

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
POST /_ccm/switch/{model-or-profile-or-route}
```

`/_ccm/models` exposes model cost/quality metadata. `/_ccm/status` and `/_ccm/routes` expose selection strategy, scoring weights, fallback policy, and circuit-breaker policy. `/_ccm/scores` explains the active route's current candidate scores. `/_ccm/decisions` returns the most recent complete per-request routing decisions. `/_ccm/clients` lists per-client runtime targets; `/_ccm/status`, `/_ccm/traces`, and `/_ccm/decisions` accept a `?client=<id>` filter. `/_ccm/decisions?since=&until=&model=` (unix-ms, inclusive) reads the persisted history from disk instead of the in-memory ring — oldest-first (chronological) and bounded to the most recent 1000 records by default, `?limit=` adjusts (v0.4 M5; a 400 names the cause when no history store is running). `/_ccm/usage` (v0.4 M6) lists captured usage records with the same parameter contract, and `/_ccm/cost?day=YYYY-MM-DD` aggregates one UTC day's tokens and USD cost by model (always disk-backed). `/metrics` (v0.4 M7) is the Prometheus text exposition — same listener, so the loopback-only bind guard covers it; `prometheus_enabled = false` unregisters the route.

The proxy binds to loopback addresses only: `ccm proxy --bind` rejects non-loopback addresses because the control API is unauthenticated.

## Routing Decision Trace

Routing Decision Trace is the final major v0.3 routing feature. CCM keeps the most recent 100 decisions in proxy memory. Since v0.4 M5 every decision also persists to append-only JSONL under `$CCM_HOME/history/` (rotation + 14-day retention, single-writer lock per `CCM_HOME`) alongside periodic metric snapshots and circuit transitions — `ccm history decisions|metrics|circuit` reads them offline, and persisted lines are whitelist serde structs that never contain credential material. Runtime metrics still restart from zero on purpose: stale history must not distort healthiest/weighted ordering.

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

Attempt traces stay intentionally in-memory (last 100, proxy lifetime). Routing decisions persist to JSONL history since v0.4 M5 (see the Control API section). Since v0.4 M6, accepted responses also persist one usage record each (`usage.jsonl`), joined to their decision id: `/_ccm/usage` lists records, `/_ccm/cost?day=YYYY-MM-DD` and `ccm history cost` aggregate tokens and USD cost by model against the hand-entered `[models.<name>.pricing]` tables (unpriced models count separately, never guessed).

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

A second mock-provider test, `mock_openai_provider_integration_covers_translation_contract`, covers the openai-compatible translation contract:

```text
translated upstream body (system message, tool mapping, no cache_control/thinking, stream_options)
Authorization: Bearer on /v1/chat/completions
Anthropic SSE translation end to end (message_start ... message_stop)
mixed-protocol 429 fallback (openai primary -> anthropic fallback)
terminal error body translation (status preserved)
committed-stream failure -> error event, zero requests to the fallback (no mid-stream failover)
HALF_OPEN probe release on translate failure
```

The twelve integration tests that mutate process env (`CCM_HOME`, `CCM_<PROVIDER>_API_KEY`) serialize on a shared lock, so they cannot race. The tests isolate configuration with `CCM_HOME`, so they do not depend on a developer's real `~/.ccm` files or OS keyring. The disk-query tests (`/_ccm/decisions`, `/_ccm/usage`, `/_ccm/cost`) use explicit history directories and need no env lock.

Run them with:

```bash
cargo test mock_provider_integration_covers_v03_routing_contract -- --nocapture
cargo test mock_openai_provider_integration_covers_translation_contract -- --nocapture
cargo test multi_client_integration_covers_scoped_switching_contract -- --nocapture
cargo test control_router_serves_client_contract_over_http -- --nocapture
```

## v0.3 stabilization boundary

v0.3.0 shipped on 2026-10-03. The stabilization release work is complete:

```text
release verification on Windows / Linux
CI gate green on every push to main
release packaging (Windows .exe + Linux binary + checksums)
```

v0.4.0 shipped on 2026-10-03 with the same packaging and gate. The v0.4
scope (multi-client routing, openai-compatible translation, observability
persistence) and its milestone-by-milestone landing records live in
`docs/V0.4_PLAN.md`.

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

The skill runs `ccm switch`, which inherits the session's `CCM_CLIENT_ID`, so a `/switch` moves only that session's target (v0.4) — never the global default other terminals follow.

## Commands

```text
ccm init [--force]
ccm add provider <name> [--base-url URL]
  [--kind anthropic|anthropic-compatible|openai-compatible]
  [--auth x-api-key|bearer]  (default resolves by kind)
ccm add model <name> [--provider PROVIDER] [--model-id MODEL]
  [--cost-weight N]
  [--quality-weight N]
ccm add route <name> [--primary MODEL] [--fallback MODEL1,MODEL2]
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
  [--client <id>] [--global]
ccm clients [--proxy-url URL]
ccm history decisions [--since MS] [--until MS] [--model M] [--client ID] [--limit N]
ccm history metrics [--limit N]
ccm history circuit [--model M] [--limit N]
ccm history cost [--day YYYY-MM-DD] [--client ID]
ccm advise [--window DAYS] [--min-samples N] [--model M]   (v0.5)
ccm discover [provider] [--all]                              (v0.5)
ccm run [model-or-profile] [--proxy] [--proxy-url URL] [--client <id>]
ccm auth set <provider>
ccm auth delete <provider>
ccm health <model-or-profile>
```

## Release verification

The release gate uses the same four Rust checks on every supported platform:

```text
cargo fmt --all -- --check
cargo check --all-targets
cargo test --all-targets
cargo clippy --all-targets -- -D warnings
```

Convenience scripts:

```bash
# Linux / macOS
./scripts/verify.sh
./scripts/smoke.sh
```

```powershell
# Windows PowerShell
./scripts/verify.ps1
./scripts/smoke.ps1
```

The smoke test uses an isolated temporary `CCM_HOME` and verifies:

```text
ccm init
config.toml creation
state.toml creation
ccm current
ccm list
Mock Provider routing integration
```

The repository pins the stable Rust channel and installs `rustfmt` and `clippy` through `rust-toolchain.toml`.

### Supported release platforms

The v0.5 release target is:

```text
Windows  x86_64-pc-windows-msvc     verified 2026-10-10
Linux    x86_64-unknown-linux-gnu   verified 2026-10-10 (WSL2 Ubuntu 24.04)
```

macOS is not claimed as supported for v0.5: there is no macOS host to run the release gate on. `install.sh` is expected to work on macOS, but that expectation is unverified, and the macOS keyring backend has never been compiled. macOS support can be re-claimed only after the gate passes on real macOS hardware or a GitHub Actions macOS runner, and must then be labeled CI-verified rather than manually verified.

Credential storage is configured per target:

```text
Windows → Windows Credential Manager
Linux   → Linux keyutils + synchronous Secret Service
macOS   → Apple Keychain (untested; macOS is not a verified v0.5 platform)
```

The Linux keyring backends compile and the credential resolution path is tested with environment credentials; interactive `ccm auth set` against a desktop Secret Service has not been exercised.

Headless environments can bypass the native keyring with `CCM_<PROVIDER>_API_KEY`.

The cross-platform scripts and platform-specific dependency configuration are in place. A release is not considered verified until these scripts have actually passed on Windows and Linux.

## Development

```bash
cargo fmt --all -- --check
cargo check --all-targets
cargo test --all-targets
cargo clippy --all-targets -- -D warnings
```

GitHub Actions CI runs the release gate ([.github/workflows/ci.yml](.github/workflows/ci.yml), ubuntu-latest + windows-latest) on every push to `main` and via manual `workflow_dispatch`.

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
- `openai-compatible` provider kind: full upstream translation between the
  Anthropic `/v1/messages` client protocol and the OpenAI
  `chat/completions` upstream protocol (requests, streaming and
  non-streaming responses, error bodies), including mixed-protocol routes
  (anthropic and openai-compatible candidates in one fallback chain)
- in-memory traces, metrics, decisions, and circuit-state control APIs
- JSONL observability history (v0.4): decisions, metric snapshots, and
  circuit transitions persist under `$CCM_HOME/history/` with rotation,
  14-day retention, and a single-writer lock; offline `ccm history` reads
  and disk-backed `/_ccm/decisions?since=&until=&model=` queries
- usage capture and cost accounting (v0.4): one usage record per accepted
  response, joined to its decision id and tagged with the client id;
  per-MTok pricing tables hand-entered per model; `/_ccm/usage`,
  `/_ccm/cost?day=`, and offline `ccm history cost` aggregate tokens and
  USD by model (unpriced models counted separately, never guessed)
- Prometheus `/metrics` (v0.4): hand-rendered text exposition on the same
  loopback-only listener — request/attempt outcomes, dual-track latency
  (fixed-bucket histogram + EWMA gauge), decision duration, circuit state,
  tokens, and micro-USD cost; `prometheus_enabled` config flag, default on
- `ccm advise` (v0.5): cost_weight suggestions derived from realized
  spend in `usage.jsonl` recomputed against the current hand-entered
  pricing tables — analysis printed, never written back to config
- persistent client sessions (v0.5): scoped per-client targets survive
  proxy restarts via best-effort `clients.toml` (TTL 7 days, LRU cap
  256, revalidated at startup; `persist = false` restores memory-only)
- `POST /v1/messages/count_tokens` forwarding (v0.5): side-effect-free
  target resolution, primary-only single attempt, status+body
  passthrough with a dedicated 10s timeout; openai-compatible primaries
  answer an honest 404 envelope
- OTLP/HTTP JSON metrics export (v0.5): the same 12 metric families
  pushed to a collector (`[observability.otlp]`, cumulative
  temporality, failed pushes not retried, credentials via
  `OTEL_EXPORTER_OTLP_HEADERS` env only)
- `ccm discover` (v0.5): list a gateway's `GET /v1/models` and register
  selected ids as models — lenient two-shape parsing, skip-not-overwrite
  on registered pairs, alias collision-suffixing, pricing skeleton
  printout for the advise journey
- no mid-stream failover

## License

MIT
