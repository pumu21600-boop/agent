# package_release.ps1 - build a self-contained Windows release folder (and zip).
#
# Layout produced (agent.exe resolves everything relative to its own directory):
#   agent-<version>-win64\
#     agent.exe            service binary (start/stop/status self-management built in)
#     config.toml          server/gateway/approval/sandbox/context/recovery/memory config
#     providers.json       provider presets
#     prompts\system.md    system conventions (edited from the UI)
#     web\dist\            built frontend (served by the exe)
#     plugins\             wasm/wat plugin examples (optional)
#     ui-ext\              UI slot extensions (optional, copied when present)
#     tools\git-runtime.zip bundled MinGit (optional; run scripts\bundle_git.ps1 first)
#     start.bat stop.bat   double-click entries
#     README.md LICENSE
#
# Usage:
#   powershell -ExecutionPolicy Bypass -File scripts\package_release.ps1
#   powershell -ExecutionPolicy Bypass -File scripts\package_release.ps1 -SkipBuild -SkipZip
#
# NOTE: keep this file ASCII-only (PS 5.1 reads .ps1 in the system ANSI codepage).

param(
    [switch]$SkipBuild,
    [switch]$SkipFrontend,
    [switch]$SkipZip,
    [string]$OutRoot = "dist"
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

function Info($m) { Write-Host "[package] $m" -ForegroundColor Cyan }
function Warn($m) { Write-Host "[package] WARN: $m" -ForegroundColor Yellow }

# --- version from Cargo.toml (single source: the crate version) ---
$cargo = Get-Content (Join-Path $root "Cargo.toml") -TotalCount 6
$version = ($cargo | Select-String -Pattern '^version\s*=\s*"([^"]+)"').Matches[0].Groups[1].Value
if (-not $version) { throw "cannot read version from Cargo.toml" }
Info "version = $version"

# --- 1. release binary ---
if (-not $SkipBuild) {
    Info "cargo build --release"
    cargo build --release
    if ($LASTEXITCODE -ne 0) { throw "cargo build --release failed" }
}
$exe = Join-Path $root "target\release\agent.exe"
if (-not (Test-Path $exe)) { throw "target\release\agent.exe not found (run without -SkipBuild)" }

# --- 2. frontend ---
$distSrc = Join-Path $root "web\dist"
if (-not $SkipFrontend) {
    Info "npm run build (web)"
    Push-Location (Join-Path $root "web")
    if (-not (Test-Path "node_modules")) { npm ci }
    npm run build
    $code = $LASTEXITCODE
    Pop-Location
    if ($code -ne 0) { throw "frontend build failed" }
}
if (-not (Test-Path (Join-Path $distSrc "index.html"))) { throw "web\dist\index.html missing" }

# --- 3. assemble ---
$name = "agent-$version-win64"
$out = Join-Path $root (Join-Path $OutRoot $name)
if (Test-Path $out) { Remove-Item $out -Recurse -Force }
New-Item -ItemType Directory -Path $out -Force | Out-Null

Copy-Item $exe (Join-Path $out "agent.exe")
New-Item -ItemType Directory -Path (Join-Path $out "web") -Force | Out-Null
Copy-Item $distSrc (Join-Path $out "web\dist") -Recurse -Force

foreach ($item in @("config.toml", "providers.json", "README.md", "LICENSE")) {
    $p = Join-Path $root $item
    if (Test-Path $p) { Copy-Item $p $out } else { Warn "missing $item" }
}
foreach ($dir in @("prompts", "plugins", "ui-ext")) {
    $p = Join-Path $root $dir
    if (Test-Path $p) { Copy-Item $p (Join-Path $out $dir) -Recurse -Force }
}

# bundled MinGit (skip quietly when the project has not fetched it)
$zip = Join-Path $root "runtime\git-runtime.zip"
if (Test-Path $zip) {
    New-Item -ItemType Directory -Path (Join-Path $out "tools") -Force | Out-Null
    Copy-Item $zip (Join-Path $out "tools\git-runtime.zip")
    Info ("bundled git: {0:N1} MB" -f ((Get-Item $zip).Length / 1MB))
} else {
    Warn "runtime\git-runtime.zip not found - release will require a system git"
    Warn "run: powershell -ExecutionPolicy Bypass -File scripts\bundle_git.ps1"
}

# --- 4. launch entries (ASCII only) ---
$startBat = @"
@echo off
rem Start the local agent service in the background and open the browser.
cd /d "%~dp0"
if not exist "agent.exe" (
  echo [start] agent.exe not found.
  pause
  exit /b 1
)
agent.exe start
"@
$stopBat = @"
@echo off
rem Stop the running agent service.
cd /d "%~dp0"
if not exist "agent.exe" (
  echo [stop] agent.exe not found. Nothing to stop.
  pause
  exit /b 1
)
agent.exe stop
pause
"@
Set-Content -Path (Join-Path $out "start.bat") -Value $startBat -Encoding ASCII
Set-Content -Path (Join-Path $out "stop.bat") -Value $stopBat -Encoding ASCII

# --- 5. zip ---
if (-not $SkipZip) {
    $zipOut = Join-Path $root (Join-Path $OutRoot "$name.zip")
    if (Test-Path $zipOut) { Remove-Item $zipOut -Force }
    Info "compressing $name.zip"
    Compress-Archive -Path (Join-Path $out "*") -DestinationPath $zipOut -CompressionLevel Optimal
    Info ("zip: {0:N1} MB" -f ((Get-Item $zipOut).Length / 1MB))
}

Info "done -> $out"
