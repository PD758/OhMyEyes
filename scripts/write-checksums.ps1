[CmdletBinding()]
param(
    [string]$DistPath = (Join-Path $PSScriptRoot "..\dist")
)

$ErrorActionPreference = "Stop"

$dist = (Resolve-Path -LiteralPath $DistPath).Path
$artifacts = Get-ChildItem -LiteralPath $dist -File |
    Where-Object { $_.Extension -eq ".zip" -or $_.Name -like "*setup.exe" } |
    Sort-Object -Property Name

if ($artifacts.Count -eq 0) {
    throw "No release artifacts found in $dist"
}

[string[]]$lines = foreach ($artifact in $artifacts) {
    $hash = (Get-FileHash -LiteralPath $artifact.FullName -Algorithm SHA256).Hash.ToLowerInvariant()
    "$hash  $($artifact.Name)"
}

$output = Join-Path $dist "SHA256SUMS.txt"
[System.IO.File]::WriteAllLines($output, $lines, [System.Text.Encoding]::ASCII)
Write-Host "Wrote SHA-256 checksums to $output"
