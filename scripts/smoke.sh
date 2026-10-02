#!/usr/bin/env sh
set -eu

TMP_ROOT="$(mktemp -d)"
cleanup() {
  rm -rf "$TMP_ROOT"
}
trap cleanup EXIT INT TERM

export CCM_HOME="$TMP_ROOT"

cargo run --quiet -- init --force
test -f "$CCM_HOME/config.toml"
test -f "$CCM_HOME/state.toml"

CURRENT="$(cargo run --quiet -- current)"
test "$CURRENT" = "claude"

cargo run --quiet -- list >/dev/null
cargo test mock_provider_integration_covers_v03_routing_contract -- --nocapture

echo "CCM platform smoke passed"
