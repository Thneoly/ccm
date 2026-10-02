param(
    [string]$Destination = (Join-Path $env:LOCALAPPDATA "Programs\ccm")
)

$ErrorActionPreference = "Stop"

$Destination = $Destination.TrimEnd('\')

cargo build --release
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

New-Item -ItemType Directory -Path $Destination -Force | Out-Null
Copy-Item target\release\ccm.exe -Destination $Destination -Force

$userPath = [Environment]::GetEnvironmentVariable("Path", "User")
if ($null -eq $userPath) { $userPath = "" }
if (($userPath -split ";" | ForEach-Object { $_.Trim() }) -notcontains $Destination) {
    $newPath = if ($userPath) { "$userPath;$Destination" } else { $Destination }
    [Environment]::SetEnvironmentVariable("Path", $newPath, "User")
    Write-Host "Added $Destination to the user PATH"
}

$installed = Join-Path $Destination "ccm.exe"
& $installed --version
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

Write-Host "Installed: $installed"
Write-Host "Open a new terminal for the PATH change to take effect."
