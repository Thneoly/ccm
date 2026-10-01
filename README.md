# CCM

A fast local model manager and control plane for AI coding CLIs, starting with Claude Code.

CCM manages providers, models, profiles, routes, and route policies locally. Claude Code can stay connected to one localhost endpoint while CCM switches the backend or route underneath it.

## Quick start

```bash
git clone https://github.com/Thneoly/ccm.git
cd ccm
cargo build --release
./target/release/ccm init
```

Store credentials and check the setup:

```bash
ccm auth set anthropic
ccm auth set zai
ccm doctor
```

## Direct mode

```bash
ccm use glm
ccm run
```

Direct mode resolves models and profiles only. Routes are a proxy-mode concept because they may contain fallback chains and policies.

## Proxy mode

```bash
ccm proxy
ccm run --proxy
```

Switch a model, profile, or route at runtime:

```bash
ccm switch glm
ccm switch fast
ccm switch coding-route
```

The runtime target is kept in memory and does not rewrite `config.toml`.

## Route layer

```toml
[routes.coding-route]
primary = "claude"
fallback = ["glm", "kimi"]

[routes.coding-route.policy]
header_timeout_ms = 30000
fallback_on = [429, 502, 503, 504]
max_attempts = 3
backoff_ms = 200
```

The architecture is:

```text
Provider
   ↓
Model
   ↓
Route (primary + fallback[])
   ↓
RoutePolicy
   ↓
Runtime Target
   ↓
Routing Policy Executor
   ↓
CCM Proxy
```

## Route Policy

Each route may configure four execution controls:

```toml
[routes.coding.policy]
header_timeout_ms = 30000
fallback_on = [429, 502, 503, 504]
max_attempts = 3
backoff_ms = 200
```

Meaning:

- `header_timeout_ms`: maximum time to wait for upstream response headers for one attempt.
- `fallback_on`: HTTP status codes that are eligible for fallback.
- `max_attempts`: total attempts including the primary attempt. It is not the number of fallbacks.
- `backoff_ms`: delay before trying the next candidate after a fallback-triggering failure.

Connection/request errors and response-header timeouts are also fallback-eligible when another candidate is available.

If a route omits `[routes.<name>.policy]`, CCM uses:

```toml
header_timeout_ms = 30000
fallback_on = [429, 502, 503, 504]
max_attempts = 3
backoff_ms = 200
```

For a route with:

```text
primary = claude
fallback = [glm, kimi, local]
max_attempts = 2
```

CCM will try at most:

```text
claude → glm
```

The remaining candidates are not attempted for that request.

Once an upstream response is accepted, CCM streams it directly back to Claude Code. If that stream fails after it has started, CCM does not switch models mid-stream.

## Creating policy-aware routes from CLI

```bash
ccm add route coding \
  --primary claude \
  --fallback glm,kimi \
  --header-timeout-ms 30000 \
  --fallback-on 429,502,503,504 \
  --max-attempts 3 \
  --backoff-ms 200
```

Then:

```bash
ccm switch coding
```

## Attempt tracing

Example stderr output:

```text
ccm route=coding attempt=1 model=claude result=HTTP 429 Too Many Requests action=fallback
ccm route=coding attempt=2 model=glm result=HTTP 200 OK
```

The proxy keeps the most recent 100 attempt traces in memory.

## Control API

```text
GET  /health
GET  /_ccm/status
GET  /_ccm/models
GET  /_ccm/routes
GET  /_ccm/traces
POST /_ccm/switch/{model-or-profile-or-route}
```

Examples:

```bash
curl http://127.0.0.1:13521/_ccm/status
curl http://127.0.0.1:13521/_ccm/routes
curl http://127.0.0.1:13521/_ccm/traces
```

The status and route endpoints expose the effective route policy. A status response looks like:

```json
{
  "target": "coding-route",
  "primary": "claude",
  "model_id": "claude-sonnet-4-5",
  "provider": "anthropic",
  "fallback": ["glm"],
  "policy": {
    "header_timeout_ms": 30000,
    "fallback_on": [429, 502, 503, 504],
    "max_attempts": 3,
    "backoff_ms": 200
  }
}
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

## Configuration example

```toml
current = "claude"

[providers.anthropic]
kind = "anthropic"
base_url = "https://api.anthropic.com"

[providers.zai]
kind = "anthropic-compatible"
base_url = "https://api.z.ai/api/anthropic"

[models.claude]
provider = "anthropic"
model_id = "claude-sonnet-4-5"

[models.glm]
provider = "zai"
model_id = "glm-5"

[profiles.fast]
model = "glm"

[routes.coding-route]
primary = "claude"
fallback = ["glm"]

[routes.coding-route.policy]
header_timeout_ms = 30000
fallback_on = [429, 502, 503, 504]
max_attempts = 3
backoff_ms = 200
```

Secrets are stored with the Rust `keyring` crate and are not written to `config.toml`.

## Current proxy scope

- Anthropic-compatible `/v1/messages`
- request model rewriting
- provider credential injection
- streaming upstream responses
- in-memory runtime target
- first-class route objects
- configurable per-route policy
- ordered route fallback execution
- response-header timeout
- status-based fallback
- max-attempt limiting
- fixed backoff between fallback attempts
- in-memory attempt tracing
- no mid-stream failover
- no OpenAI protocol translation yet

## Development

```bash
cargo fmt --all -- --check
cargo check --all-targets
cargo test --all-targets
cargo clippy --all-targets -- -D warnings
```

GitHub Actions CI is currently disabled; run these checks locally before release commits.

## Next

The next useful routing step is health-aware execution: per-model failure counters, cooldown windows, and simple circuit breaking so repeatedly failing candidates can be skipped temporarily.

## License

MIT
