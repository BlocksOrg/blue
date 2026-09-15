# Run from a fresh Windows test account (GitHub hosted runner or disposable VM).
$ErrorActionPreference = 'Stop'
if ($env:OS -ne 'Windows_NT') { throw 'Native Windows is required; WSL does not exercise ConPTY.' }
# Blue uses Known Folder APIs, not overridden environment paths. Refuse an
# environment where the suite's cleanup paths would differ from Blue's paths.
if ($env:USERPROFILE -ne [Environment]::GetFolderPath('UserProfile') -or
    $env:LOCALAPPDATA -ne [Environment]::GetFolderPath('LocalApplicationData')) {
    throw 'USERPROFILE/LOCALAPPDATA must match Windows Known Folders.'
}
$repo = (Resolve-Path (Join-Path $PSScriptRoot '../..')).Path
Push-Location $repo
try {
    cargo build --locked -p gh-cli
    if ($LASTEXITCODE -ne 0) { throw 'Blue build failed' }
    $metadata = cargo metadata --no-deps --format-version 1 | ConvertFrom-Json
    if ($LASTEXITCODE -ne 0) { throw 'Cargo metadata failed' }
    $env:E2E_WINDOWS_BLUE_BIN = Join-Path $metadata.target_directory 'debug/blue.exe'
    cargo test --locked --manifest-path tests/e2e-windows/Cargo.toml --test launch -- --nocapture --test-threads=1
    if ($LASTEXITCODE -ne 0) { throw 'Windows E2E failed' }
} finally {
    Pop-Location
}
