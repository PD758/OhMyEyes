param(
    [switch]$SkipBuild
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
$versionLine = Select-String -LiteralPath (Join-Path $root "Cargo.toml") -Pattern '^version = "([^"]+)"$' | Select-Object -First 1
if (-not $versionLine) {
    throw "Could not read the package version from Cargo.toml."
}
$version = $versionLine.Matches[0].Groups[1].Value

if (-not $SkipBuild) {
    & (Join-Path $root "build-local.bat") release
    if ($LASTEXITCODE -ne 0) {
        throw "Release build failed with exit code $LASTEXITCODE."
    }
}

$binary = Join-Path $root "target\release\ohmyeyes.exe"
if (-not (Test-Path -LiteralPath $binary)) {
    throw "Release binary not found at $binary."
}

$dist = Join-Path $root "dist"
$portable = Join-Path $dist "OhMyEyes-$version-windows-x64"
if (Test-Path -LiteralPath $portable) {
    Remove-Item -LiteralPath $portable -Recurse -Force
}
New-Item -ItemType Directory -Force -Path $portable | Out-Null
Copy-Item -LiteralPath $binary -Destination $portable -Force
Copy-Item -LiteralPath (Join-Path $root "README.md") -Destination $portable -Force
Copy-Item -LiteralPath (Join-Path $root "LICENSE") -Destination $portable -Force

$archive = Join-Path $dist "OhMyEyes-$version-windows-x64-portable.zip"
Compress-Archive -Path (Join-Path $portable "*") -DestinationPath $archive -Force
Remove-Item -LiteralPath $portable -Recurse -Force
Write-Output $archive
