$ErrorActionPreference = "Stop"

$Repository = if ($env:BLUE_REPOSITORY) { $env:BLUE_REPOSITORY } else { "BlocksOrg/blue" }
$InstallDir = if ($env:BLUE_INSTALL_DIR) { $env:BLUE_INSTALL_DIR } else { Join-Path $env:LOCALAPPDATA "Blue\bin" }
$Version = $env:BLUE_VERSION

$Architecture = [System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture.ToString()
$Target = switch ($Architecture) {
    "X64" { "x86_64-pc-windows-msvc" }
    "Arm64" { "aarch64-pc-windows-msvc" }
    default { throw "blue installer: unsupported Windows architecture $Architecture (x64 and ARM64 are supported)" }
}
if (-not $Version) {
    $release = Invoke-RestMethod -Headers @{ "User-Agent" = "blue-cli-installer" } `
        -Uri "https://api.github.com/repos/$Repository/releases/latest"
    $Version = $release.tag_name
}
if (-not $Version.StartsWith("v")) { $Version = "v$Version" }

$Asset = "blue-$Version-$Target.zip"
$BaseUrl = "https://github.com/$Repository/releases/download/$Version"
$TempDir = Join-Path ([System.IO.Path]::GetTempPath()) ("blue-cli-" + [guid]::NewGuid())
New-Item -ItemType Directory -Path $TempDir | Out-Null

try {
    $Archive = Join-Path $TempDir $Asset
    $Checksums = Join-Path $TempDir "SHA256SUMS"
    Invoke-WebRequest -UseBasicParsing -Uri "$BaseUrl/$Asset" -OutFile $Archive
    Invoke-WebRequest -UseBasicParsing -Uri "$BaseUrl/SHA256SUMS" -OutFile $Checksums

    $line = Get-Content $Checksums | Where-Object { $_ -match "^[0-9a-fA-F]{64}\s+$([regex]::Escape($Asset))$" } | Select-Object -First 1
    if (-not $line) { throw "blue installer: $Asset is missing from SHA256SUMS" }
    $Expected = ($line -split "\s+")[0].ToLowerInvariant()
    $Actual = (Get-FileHash -Algorithm SHA256 $Archive).Hash.ToLowerInvariant()
    if ($Actual -ne $Expected) { throw "blue installer: checksum verification failed for $Asset" }

    Expand-Archive -Path $Archive -DestinationPath $TempDir -Force
    $Binary = Join-Path $TempDir "blue.exe"
    if (-not (Test-Path $Binary)) { throw "blue installer: archive does not contain blue.exe" }
    New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
    Move-Item -Force $Binary (Join-Path $InstallDir "blue.exe")

    Write-Host "Installed Blue $Version to $InstallDir\blue.exe"
    Write-Host "The installer does not modify PATH. Add this directory to your user PATH:"
    Write-Host "  $InstallDir"
    Write-Host "For this PowerShell session only, run:"
    Write-Host ('  $env:Path += ";{0}"' -f $InstallDir)
}
finally {
    Remove-Item -Recurse -Force -ErrorAction SilentlyContinue $TempDir
}
