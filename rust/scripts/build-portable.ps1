# Builds single-file datv-tx.exe (window) and datv-tx-cli.exe for 64-bit Windows that need
# nothing installed besides FFmpeg: the C runtime is linked statically. The results go to
# rust\exe (git-ignored). Builds in target\portable, so the usual target directory stays.
#
#   powershell -ExecutionPolicy Bypass -File scripts\build-portable.ps1
$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
$triple = "x86_64-pc-windows-msvc"
$env:RUSTFLAGS = "-C target-feature=+crt-static"
$env:CARGO_TARGET_DIR = Join-Path $root "target\portable"
Push-Location $root
try {
    cargo build --release --target $triple
    if ($LASTEXITCODE -ne 0) { throw "cargo build failed" }
    New-Item -ItemType Directory -Force (Join-Path $root "exe") | Out-Null
    foreach ($exe in @("datv-tx.exe", "datv-tx-cli.exe")) {
        Copy-Item (Join-Path $env:CARGO_TARGET_DIR "$triple\release\$exe") (Join-Path $root "exe\$exe") -Force
        Write-Host "exe\$exe"
    }
} finally { Pop-Location }
