$ErrorActionPreference = "Stop"

cargo fmt --all -- --check
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

cargo check --all-targets
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

cargo test --all-targets
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

cargo clippy --all-targets -- -D warnings
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
