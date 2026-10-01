# CCM

A fast local model manager for AI coding CLIs, starting with Claude Code.

CCM manages provider/model aliases locally and can run as a local Anthropic-compatible proxy, so Claude Code can stay connected to one localhost endpoint while CCM switches the backend underneath it.

## Quick start

```bash
git clone https://github.com/Thneoly/ccm.git
cd ccm
cargo build --release
./target/release/ccm init
```

Store provider credentials in the operating-system keyring:

```bash
./target/release/ccm auth set anthropic
./target/release/ccm auth set zai
```

Check the setup:

```bash
./target/release/ccm doctor
```

## Direct mode

Launch Claude Code directly against the selected provider:

```bash
ccm use glm
ccm run
```

Or select a model for one launch:

```bash
ccm run claude
```

## Proxy mode

Start the local router in terminal 1:

```bash
ccm proxy
```

By default it listens on:

```text
http://127.0.0.1:13521
```

Launch Claude Code through the proxy in terminal 2:

```bash
ccm run --proxy
```

Now switch backends without restarting the proxy or changing the Claude Code endpoint:

```bash
ccm use glm
ccm use claude
ccm use fast
```

The proxy reloads `~/.ccm/config.toml` on every request. It rewrites the request `model` field to the currently selected CCM model, injects the selected provider credential, forwards the request to the configured Anthropic-compatible `/v1/messages` endpoint, and streams the upstream response back to Claude Code.

You can also switch and launch in one command:

```bash
ccm run glm --proxy
```

Use another local port if needed:

```bash
ccm proxy --bind 127.0.0.1:14521
ccm run --proxy --proxy-url http://127.0.0.1:14521
```

## In-session switching from Claude Code

Install CCM's global Claude Code command:

```bash
ccm integrate claude
```

This creates:

```text
~/.claude/commands/switch.md
```

Then start Claude Code through CCM's proxy:

```bash
ccm proxy
ccm run --proxy
```

Inside the running Claude Code session, switch backends with:

```text
/switch glm
/switch claude
/switch fast
```

The slash command runs `ccm use <model-or-profile>`. Because the proxy reloads CCM configuration on each request, subsequent Claude Code requests are routed to the newly selected backend without restarting the session.

Remove the integration with:

```bash
ccm integrate claude --remove
```

The integration expects the `ccm` executable to be available on `PATH`.

## Commands

```text
ccm init [--force]
ccm add provider <name> [--base-url URL] [--kind KIND]
ccm add model <name> [--provider PROVIDER] [--model-id MODEL]
ccm integrate claude [--remove]
ccm doctor
ccm proxy [--bind HOST:PORT]
ccm list
ccm current
ccm use <model-or-profile>
ccm run [model-or-profile] [--proxy] [--proxy-url URL]
ccm auth set <provider>
ccm auth delete <provider>
ccm health <model-or-profile>
```

## Add providers and models

```bash
ccm add provider moonshot \
  --kind anthropic-compatible \
  --base-url https://api.moonshot.ai/anthropic

ccm add model kimi \
  --provider moonshot \
  --model-id kimi-k2.5
```

Missing values are prompted interactively:

```bash
ccm add provider moonshot
ccm add model kimi
```

## Doctor and health

`ccm doctor` checks local configuration, Claude Code availability, selected model/provider, credential presence, and basic endpoint reachability.

For an authenticated model request, use:

```bash
ccm health glm
```

## Configuration

CCM reads `~/.ccm/config.toml`.

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
```

Secrets are stored with the Rust `keyring` crate and are not written to `config.toml`.

## Proxy scope

The current proxy intentionally stays small:

- Anthropic-compatible `/v1/messages` only
- request model rewriting
- provider credential injection
- streaming upstream responses
- dynamic routing via the current CCM model
- no OpenAI protocol translation yet

OpenAI-compatible adapters, fallback chains, retries, and routing policies are deferred to later versions.

## Development

```bash
cargo fmt --all -- --check
cargo check --all-targets
cargo test --all-targets
cargo clippy --all-targets -- -D warnings
```

GitHub Actions runs these checks on Linux, Windows, and macOS.

## Roadmap

### v0.2

- Local Axum proxy
- Stable localhost endpoint for Claude Code
- In-session backend switching
- Streaming Anthropic-compatible forwarding
- Proxy-aware Claude Code launcher
- Global `/switch` integration for Claude Code

### v0.3

- Route aliases such as `coding`, `fast`, `cheap`, `local`
- Fallback chains
- Retry/timeout policies
- Optional OpenAI-compatible protocol adapter

## Design principles

1. CLI first; TUI later.
2. Aliases over raw provider model IDs.
3. Secrets never stored in `config.toml`.
4. Prefer transparent Anthropic-compatible forwarding before protocol translation.
5. Keep the proxy small and inspectable.

## License

MIT
