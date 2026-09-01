param(
    [string]$Binary = "target\release\ohmyeyes.exe",
    [int]$VisibleSeconds = 8
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
$binaryPath = Join-Path $root $Binary
if (-not (Test-Path -LiteralPath $binaryPath)) {
    throw "Binary not found at $binaryPath."
}

$process = $null
try {
    $process = Start-Process -FilePath $binaryPath -PassThru
    Start-Sleep -Seconds $VisibleSeconds
    $process.Refresh()
    if ($process.HasExited) {
        throw "The settings instance exited unexpectedly with code $($process.ExitCode)."
    }
    if (-not $process.Responding) {
        throw "The settings process is not responding to Windows messages."
    }
    Write-Output "Settings smoke test passed: PID $($process.Id) is responding."
}
finally {
    if ($process -and -not $process.HasExited) {
        Stop-Process -Id $process.Id
        $process.WaitForExit()
    }
}
