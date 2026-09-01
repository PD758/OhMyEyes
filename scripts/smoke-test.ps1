param(
    [string]$Binary = "target\release\ohmyeyes.exe"
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
$binaryPath = Join-Path $root $Binary
if (-not (Test-Path -LiteralPath $binaryPath)) {
    throw "Binary not found at $binaryPath."
}

Add-Type @"
using System;
using System.Runtime.InteropServices;

public static class OhMyEyesSmokeTest {
    [DllImport("user32.dll")]
    public static extern bool IsWindowVisible(IntPtr window);
}
"@

$first = $null
try {
    $first = Start-Process -FilePath $binaryPath -ArgumentList "--background" -WindowStyle Hidden -PassThru
    Start-Sleep -Seconds 4
    if ($first.HasExited) {
        throw "The background instance exited unexpectedly with code $($first.ExitCode)."
    }

    $second = Start-Process -FilePath $binaryPath -ArgumentList "--background" -WindowStyle Hidden -PassThru -Wait
    if ($second.ExitCode -ne 0) {
        throw "The second instance exited with code $($second.ExitCode)."
    }
    if ($first.HasExited) {
        throw "The primary instance exited while handling second-instance activation."
    }

    $deadline = [DateTime]::UtcNow.AddSeconds(5)
    do {
        $first.Refresh()
        $settingsVisible = $first.MainWindowHandle -ne [IntPtr]::Zero -and
            [OhMyEyesSmokeTest]::IsWindowVisible($first.MainWindowHandle)
        if (-not $settingsVisible) {
            Start-Sleep -Milliseconds 100
        }
    } while (-not $settingsVisible -and [DateTime]::UtcNow -lt $deadline)
    if (-not $settingsVisible) {
        throw "The second instance did not open the primary settings window."
    }

    Write-Output "Smoke test passed: primary PID $($first.Id), activation opened settings."
}
finally {
    if ($first -and -not $first.HasExited) {
        Stop-Process -Id $first.Id
        $first.WaitForExit()
    }
}
