# CCM

A fast local model manager and control plane for AI coding CLIs, starting with Claude Code.

CCM manages providers, models, profiles, and routes locally. Claude Code can stay connected to one localhost endpoint while CCM switches the backend or route underneath it.

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

Direct mode resolves models and profiles only. Routes are a proxy-mode concept because they may contain fallback chains.

## Proxy mode

Start the local control plane:

```bash
ccm proxy
```

Launch Claude Code through it:

```bash
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

A route is a first-class object that preserves routing intent:

```toml
[routes.coding-route]
primary = "claude"
fallback = ["glm"]

[routes.fast-route]
primary = "glm"
fallback = []
```

Create one from the CLI:

```bash
ccm add route coding \
  --primary claude \
  --fallback glm,kimi
```

Then switch to it:

```bash
ccm switch coding
```

The architecture is:

```text
Provider
   ↓
Model
   ↓
Route (primary + fallback[])
   ↓
Runtime Target
   ↓
Routing Policy Executor
   ↓
CCM Proxy
```

## Fallback policy

For each request, CCM tries the route candidates in this order:

```text
primary → fallback[0] → fallback[1] → ...
```

Fallback is intentionally narrow. CCM continues to the next candidate on:

```text
connection / request error
response-header timeout (30s)
HTTP 429
HTTP 502
HTTP 503
HTTP 504
```

CCM does not automatically fallback on client/config/authentication failures such as:

```text
HTTP 400
HTTP 401
HTTP 403
HTTP 500
```

Once an upstream response is accepted, CCM streams that response directly back to Claude Code. It does not buffer the full SSE response in order to retry. If a streaming response fails after it has already started, CCM does not switch providers mid-stream.

Example route:

```toml
[routes.coding]
primary = "claude"
fallback = ["glm", "kimi"]
```

A request may produce proxy traces such as:

```text
ccm route=coding attempt=1 model=claude result=HTTP 429 action=fallback
ccm route=coding attempt=2 model=glm result=HTTP 200
```

Or:

```text
ccm route=coding attempt=1 model=claude result=timeout after 30s action=fallback
ccm route=coding attempt=2 model=glm result=HTTP 200
```

## Control API

```text
GET  /health
GET  /_ccm/status
GET  /_ccm/models
GET  /_ccm/routes
POST /_ccm/switch/{model-or-profile-or-route}
```

Examples:

```bash
curl http://127.0.0.1:13521/_ccm/status
curl http://127.0.0.1:13521/_ccm/models
curl http://127.0.0.1:13521/_ccm/routes
curl -X POST http://127.0.0.1:13521/_ccm/switch/coding-route
```

A route-aware status response looks like:

```json
{
  "target": "coding-route",
  "primary": "claude",
  "model_id": "claude-sonnet-4-5",
  "provider": "anthropic",
  "fallback": ["glm"]
}
```

## In-session switching from Claude Code

Install the global command once:

```bash
ccm integrate claude
```

Then, in a Claude Code session started through `ccm run --proxy`:

```text
/switch glm
/switch claude
/switch coding-route
```

The slash command calls `ccm switch`, which talks directly to the running CCM control API.

## Commands

```text
ccm init [--force]
ccm add provider <name> [--base-url URL] [--kind KIND]
ccm add model <name> [--provider PROVIDER] [--model-id MODEL]
ccm add route <name> --primary MODEL [--fallback MODEL1,MODEL2]
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

[profiles.coding]
model = "claude"

[profiles.fast]
model = "glm"

[routes.coding-route]
primary = "claude"
fallback = ["glm"]
```

Secrets are stored with the Rust `keyring` crate and are not written to `config.toml`.

## Current proxy scope

- Anthropic-compatible `/v1/messages`
- request model rewriting
- provider credential injection
- streaming upstream responses
- in-memory runtime target
- first-class route objects
- local control API
- ordered route fallback execution
- attempt tracing
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

The next routing step is policy configuration: make timeout/retry/fallback conditions route-specific, add backoff, and expose recent attempt traces through the control API.

## License

MIT
