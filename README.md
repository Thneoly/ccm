# CCM

A fast local model manager for AI coding CLIs, starting with Claude Code.

CCM keeps provider/model switching local and lightweight. The v0.1 scope focuses on configuration, credential management, model aliases, health checks, and launching Claude Code with the selected backend. A local proxy for in-session switching is planned for v0.2.

## Goals

- Switch Claude Code backends with short aliases such as `ccm use glm`.
- Keep API keys out of plaintext config files.
- Support multiple Anthropic-compatible providers.
- Stay local-first, fast, and easy to inspect.
- Provide a clean path toward a local routing proxy without turning v0.1 into a gateway platform.

## Quick start

```bash
git clone https://github.com/Thneoly/ccm.git
cd ccm
cargo build --release
```

Initialize a starter configuration:

```bash
./target/release/ccm init
```

Store credentials in the operating-system keyring:

```bash
./target/release/ccm auth set anthropic
./target/release/ccm auth set zai
```

Then switch and launch Claude Code:

```bash
./target/release/ccm list
./target/release/ccm use glm
./target/release/ccm run
```

You can also select a model for one launch without changing the current selection:

```bash
./target/release/ccm run claude
```

## v0.1 commands

```text
ccm init [--force]
ccm list
ccm current
ccm use <model-or-profile>
ccm run [model-or-profile]
ccm auth set <provider>
ccm auth delete <provider>
ccm health <model-or-profile>
```

## Configuration

CCM reads `~/.ccm/config.toml` by default. `ccm init` creates a starter configuration containing Anthropic and Z.AI examples.

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

The same starter configuration is available at `examples/config.toml`.

## Credentials

CCM uses the operating-system keyring through the Rust `keyring` crate. Secrets are not written to `config.toml`.

```bash
ccm auth set zai
```

Credentials use service name `ccm` and the provider name as the account key.

## Running Claude Code

```bash
ccm run glm
```

CCM resolves the model alias, retrieves the provider credential, and launches `claude` with:

```text
ANTHROPIC_BASE_URL
ANTHROPIC_AUTH_TOKEN
ANTHROPIC_MODEL
```

Profiles are aliases to model aliases, so this also works:

```bash
ccm run fast
```

## Development

```bash
cargo fmt --all -- --check
cargo check --all-targets
cargo test --all-targets
cargo clippy --all-targets -- -D warnings
```

GitHub Actions runs these checks on Linux, Windows, and macOS.

## Roadmap

### v0.1

- CLI with clap
- First-run `ccm init`
- TOML config
- OS keyring credentials
- Provider/model/profile aliases
- Claude Code launcher
- Health checks
- Cross-platform CI

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
3. Secrets never stored in `config.toml`.
4. Prefer transparent Anthropic-compatible forwarding before protocol translation.
5. Keep v0.1 small enough to understand in one sitting.

## License

MIT
