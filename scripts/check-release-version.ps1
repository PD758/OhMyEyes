$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
$versionLine = Select-String -LiteralPath (Join-Path $root "Cargo.toml") -Pattern '^version = "([^"]+)"$' | Select-Object -First 1
if (-not $versionLine) {
    throw "Could not read the package version from Cargo.toml."
}

$expectedTag = "v$($versionLine.Matches[0].Groups[1].Value)"
if ($env:GITHUB_REF_NAME -ne $expectedTag) {
    throw "Release tag '$env:GITHUB_REF_NAME' does not match package version '$expectedTag'."
}
