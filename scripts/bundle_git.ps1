# bundle_git.ps1 — fetch MinGit once, place it next to pawch-agent.exe so the product carries its own git.
#
# Usage (from repo root):
#   powershell -ExecutionPolicy Bypass -File scripts\bundle_git.ps1
#   powershell -ExecutionPolicy Bypass -File scripts\bundle_git.ps1 -OutDir dist -Force
#
# Behavior:
#   1) runtime\git-runtime.zip missing (or -Force) -> download MinGit from CN mirror, GitHub as fallback
#   2) sanity-check the zip (size + bsdtar integrity listing)
#   3) copy it to <OutDir>\tools\git-runtime.zip  (the app looks there, next to the exe)
#
# The app itself NEVER downloads anything at runtime: it uses the user's own git first (PATH /
# registry / common locations); only when none is found does it lazily extract this zip into
# %LOCALAPPDATA%\pawch-agent\runtime\git — no network, no admin rights.
# (2026-10-22 改名轮：旧路径 %LOCALAPPDATA%\agent\runtime\git 的已解压缓存兼容一个版本，见 src/tools/git.rs)
#
# NOTE: this file is intentionally ASCII-only. Windows PowerShell 5.1 parses BOM-less UTF-8
# scripts as ANSI, which corrupts non-ASCII text (a known pitfall in this project).
param(
    [string]$OutDir = "",
    [switch]$Force
)

$ErrorActionPreference = "Stop"

$root = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path
if ([string]::IsNullOrWhiteSpace($OutDir)) {
    $OutDir = Join-Path $root "target\release"
}
$runtime = Join-Path $root "runtime"
$zip = Join-Path $runtime "git-runtime.zip"

$ver = "2.55.0.5"
$asset = "MinGit-$ver-64-bit.zip"
$mirrors = @(
    "https://mirrors.huaweicloud.com/git-for-windows/v$ver/$asset",
    "https://github.com/git-for-windows/git/releases/download/v$ver/$asset"
)

if (-not (Test-Path $runtime)) {
    New-Item -ItemType Directory -Force -Path $runtime | Out-Null
}

if ($Force -or -not (Test-Path $zip)) {
    $ok = $false
    foreach ($u in $mirrors) {
        Write-Host "[bundle-git] downloading $u"
        & curl.exe -L --fail --silent --show-error -o $zip $u
        if ($LASTEXITCODE -eq 0 -and (Test-Path $zip)) {
            $ok = $true
            break
        }
        Write-Warning "[bundle-git] mirror failed: $u"
    }
    if (-not $ok) {
        throw "[bundle-git] could not download $asset from any mirror"
    }
} else {
    Write-Host "[bundle-git] reusing existing $zip (use -Force to re-download)"
}

# Sanity check: plausible size + the archive actually lists (bsdtar is bundled with Windows 10+).
$size = (Get-Item $zip).Length
if ($size -lt 30MB) {
    throw "[bundle-git] zip looks too small ($([math]::Round($size / 1MB, 1)) MB) - download likely broken"
}
& "$env:SystemRoot\System32\tar.exe" -tf $zip > $null
if ($LASTEXITCODE -ne 0) {
    throw "[bundle-git] integrity check failed: tar cannot list $zip"
}
Write-Host ("[bundle-git] runtime zip OK: {0:N1} MB" -f ($size / 1MB))

# Place next to the exe: <OutDir>\tools\git-runtime.zip
$tools = Join-Path $OutDir "tools"
New-Item -ItemType Directory -Force -Path $tools | Out-Null
Copy-Item $zip (Join-Path $tools "git-runtime.zip") -Force
Write-Host "[bundle-git] placed: $(Join-Path $tools 'git-runtime.zip')"
Write-Host "[bundle-git] done - on a machine without git it extracts to %LOCALAPPDATA%\pawch-agent\runtime\git on first use"
