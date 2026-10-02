$ErrorActionPreference = "Stop"

$TempRoot = Join-Path ([System.IO.Path]::GetTempPath()) ("ccm-smoke-" + [guid]::NewGuid().ToString("N"))
New-Item -ItemType Directory -Path $TempRoot | Out-Null
$env:CCM_HOME = $TempRoot

try {
    cargo run --quiet -- init --force
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

    if (-not (Test-Path (Join-Path $env:CCM_HOME "config.toml"))) {
        throw "config.toml was not created"
    }
    if (-not (Test-Path (Join-Path $env:CCM_HOME "state.toml"))) {
        throw "state.toml was not created"
    }

    $Current = (cargo run --quiet -- current).Trim()
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
    if ($Current -ne "claude") {
        throw "expected current target claude, got $Current"
    }

    cargo run --quiet -- list | Out-Null
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

    cargo test mock_provider_integration_covers_v03_routing_contract -- --nocapture
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

    Write-Host "CCM platform smoke passed"
}
finally {
    Remove-Item Env:CCM_HOME -ErrorAction SilentlyContinue
    Remove-Item -Recurse -Force $TempRoot -ErrorAction SilentlyContinue
}
