# CCM

A fast local model manager for AI coding CLIs, starting with Claude Code.

CCM keeps provider/model switching local and lightweight. The v0.1 scope focuses on configuration, credential management, model aliases, health checks, and launching Claude Code with the selected backend. A local proxy for in-session switching is planned for v0.2.

## Goals

- Switch Claude Code backends with short aliases such as `ccm use glm`.
- Keep API keys out of plaintext config files.
- Support multiple Anthropic-compatible providers.
- Stay local-first, fast, and easy to inspect.
- Provide a clean path toward a local routing proxy without turning v0.1 into a gateway platform.

## v0.1 commands

```bash
ccm list
ccm current
ccm use glm
ccm run glm
ccm auth set zai
ccm health glm
```

## Install

```bash
git clone https://github.com/Thneoly/ccm.git
cd ccm
cargo build --release
```

The binary will be available at:

```text
target/release/ccm
```

## Configuration

CCM reads `~/.ccm/config.toml` by default.

Example:

```toml
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

You can also start from `examples/config.toml`.

## Credentials

CCM uses the operating system keyring via the Rust `keyring` crate.

```bash
ccm auth set zai
```

The secret is stored under service `ccm` and the provider name as the account key.

## Running Claude Code

```bash
ccm run glm
```

CCM resolves the model alias, loads the provider credential, and launches `claude` with:

```text
ANTHROPIC_BASE_URL
ANTHROPIC_AUTH_TOKEN
ANTHROPIC_MODEL
```

## Roadmap

### v0.1

- CLI with clap
- TOML config
- OS keyring credentials
- Provider/model/profile aliases
- Claude Code launcher
- Health checks

### v0.2

- Local Axum proxy
- Stable localhost endpoint for Claude Code
- In-session backend switching
- `ccm proxy`

### v0.3

- Route aliases such as `coding`, `fast`, `cheap`, `local`
- Fallback chains
- Retry/timeout policies
- Optional OpenAI-compatible protocol adapter

## Design principles

1. CLI first; TUI later.
2. Aliases over raw provider model IDs.
3. Secrets never stored in config.toml.
4. Prefer transparent Anthropic-compatible forwarding before protocol translation.
5. Keep v0.1 small enough to understand in one sitting.

## License

MIT
