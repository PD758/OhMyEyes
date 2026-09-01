param(
    [string]$Binary = "target\release\ohmyeyes.exe"
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
$binaryPath = Join-Path $root $Binary
$captureDirectory = Join-Path $root "target\smoke"
if (-not (Test-Path -LiteralPath $binaryPath)) {
    throw "Binary not found at $binaryPath."
}

Add-Type @"
using System;
using System.Runtime.InteropServices;
using System.Text;

public static class OhMyEyesWindowTest {
    private delegate bool EnumWindowsCallback(IntPtr window, IntPtr parameter);

    [StructLayout(LayoutKind.Sequential)]
    public struct Rect {
        public int Left;
        public int Top;
        public int Right;
        public int Bottom;
    }

    [DllImport("user32.dll")]
    public static extern bool GetWindowRect(IntPtr window, out Rect rect);

    [DllImport("user32.dll")]
    private static extern bool EnumWindows(EnumWindowsCallback callback, IntPtr parameter);

    [DllImport("user32.dll")]
    private static extern uint GetWindowThreadProcessId(IntPtr window, out uint processId);

    [DllImport("user32.dll", CharSet = CharSet.Unicode)]
    private static extern int GetWindowText(IntPtr window, StringBuilder text, int maximumCount);

    [DllImport("user32.dll", EntryPoint = "GetWindowLongPtrW")]
    public static extern IntPtr GetWindowLongPtr(IntPtr window, int index);

    [DllImport("user32.dll", SetLastError = true)]
    public static extern IntPtr SendMessageTimeout(
        IntPtr window,
        uint message,
        IntPtr wParam,
        IntPtr lParam,
        uint flags,
        uint timeout,
        out IntPtr result
    );

    public static IntPtr FindWindow(uint expectedProcessId, string expectedTitle) {
        IntPtr found = IntPtr.Zero;
        EnumWindows(delegate(IntPtr window, IntPtr parameter) {
            uint processId;
            GetWindowThreadProcessId(window, out processId);
            if (processId != expectedProcessId) {
                return true;
            }
            var title = new StringBuilder(256);
            GetWindowText(window, title, title.Capacity);
            if (title.ToString() == expectedTitle) {
                found = window;
                return false;
            }
            return true;
        }, IntPtr.Zero);
        return found;
    }
}
"@
Add-Type -AssemblyName System.Drawing

function Save-WindowCenterCrop([OhMyEyesWindowTest+Rect]$Rect, [string]$Path) {
    $width = [Math]::Min(640, $Rect.Right - $Rect.Left)
    $height = [Math]::Min(360, $Rect.Bottom - $Rect.Top)
    $left = $Rect.Left + [Math]::Floor((($Rect.Right - $Rect.Left) - $width) / 2)
    $top = $Rect.Top + [Math]::Floor((($Rect.Bottom - $Rect.Top) - $height) / 2)
    $bitmap = New-Object System.Drawing.Bitmap($width, $height)
    $graphics = [System.Drawing.Graphics]::FromImage($bitmap)
    try {
        $graphics.CopyFromScreen($left, $top, 0, 0, $bitmap.Size)
        New-Item -ItemType Directory -Force -Path (Split-Path -Parent $Path) | Out-Null
        $bitmap.Save($Path, [System.Drawing.Imaging.ImageFormat]::Png)
    }
    finally {
        $graphics.Dispose()
        $bitmap.Dispose()
    }
}

function Get-WindowRect([IntPtr]$Window) {
    $rect = New-Object OhMyEyesWindowTest+Rect
    if (-not [OhMyEyesWindowTest]::GetWindowRect($Window, [ref]$rect)) {
        throw "GetWindowRect failed."
    }
    return $rect
}

$primary = $null
try {
    $primary = Start-Process -FilePath $binaryPath -ArgumentList "--show-now" -PassThru
    Start-Sleep -Seconds 4
    $primary.Refresh()
    if ($primary.HasExited) {
        throw "The primary instance exited before the test started."
    }
    $settingsWindow = [OhMyEyesWindowTest]::FindWindow($primary.Id, "OhMyEyes settings")
    if ($settingsWindow -eq [IntPtr]::Zero) {
        throw "The settings HWND was not found."
    }
    $transparentStyle = 0x20
    $settingsStyle = [OhMyEyesWindowTest]::GetWindowLongPtr($settingsWindow, -20).ToInt64()
    if (($settingsStyle -band $transparentStyle) -ne 0) {
        throw "The settings HWND incorrectly has WS_EX_TRANSPARENT."
    }
    $before = Get-WindowRect $settingsWindow

    $activation = Start-Process -FilePath $binaryPath -ArgumentList "--show-now" -WindowStyle Hidden -PassThru
    if (-not $activation.WaitForExit(5000)) {
        Stop-Process -Id $activation.Id
        throw "Show-now activation did not exit within five seconds."
    }
    if ($activation.ExitCode -ne 0) {
        throw "Show-now activation failed with code $($activation.ExitCode)."
    }
    Start-Sleep -Seconds 3
    $overlayWindow = [IntPtr]::Zero
    $overlayRect = $null
    $overlayDeadline = [DateTime]::UtcNow.AddSeconds(10)
    do {
        $overlayWindow = [OhMyEyesWindowTest]::FindWindow($primary.Id, "OhMyEyes reminder")
        if ($overlayWindow -ne [IntPtr]::Zero) {
            $overlayRect = Get-WindowRect $overlayWindow
        }
        if (-not $overlayRect -or
            $overlayRect.Right -le $overlayRect.Left -or
            $overlayRect.Bottom -le $overlayRect.Top) {
            Start-Sleep -Milliseconds 100
        }
    } while (
        (-not $overlayRect -or
            $overlayRect.Right -le $overlayRect.Left -or
            $overlayRect.Bottom -le $overlayRect.Top) -and
        [DateTime]::UtcNow -lt $overlayDeadline
    )
    if (-not $overlayRect -or
        $overlayRect.Right -le $overlayRect.Left -or
        $overlayRect.Bottom -le $overlayRect.Top) {
        throw "The reminder overlay did not obtain non-empty bounds within ten seconds."
    }
    $overlayStyle = [OhMyEyesWindowTest]::GetWindowLongPtr($overlayWindow, -20).ToInt64()
    if (($overlayStyle -band $transparentStyle) -eq 0) {
        throw "The reminder overlay does not have WS_EX_TRANSPARENT."
    }
    Write-Output "Overlay bounds: $($overlayRect.Left),$($overlayRect.Top) - $($overlayRect.Right),$($overlayRect.Bottom)"
    $capturePath = Join-Path $captureDirectory "overlay-crop.png"
    Save-WindowCenterCrop $overlayRect $capturePath
    $after = Get-WindowRect $settingsWindow

    $beforeWidth = $before.Right - $before.Left
    $beforeHeight = $before.Bottom - $before.Top
    $afterWidth = $after.Right - $after.Left
    $afterHeight = $after.Bottom - $after.Top
    if ($beforeWidth -ne $afterWidth -or $beforeHeight -ne $afterHeight) {
        throw "Settings changed size from ${beforeWidth}x${beforeHeight} to ${afterWidth}x${afterHeight}."
    }
    $messageResult = [IntPtr]::Zero
    $messageSucceeded = [OhMyEyesWindowTest]::SendMessageTimeout(
        $settingsWindow,
        0,
        [IntPtr]::Zero,
        [IntPtr]::Zero,
        0x2,
        2000,
        [ref]$messageResult
    )
    if ($messageSucceeded -eq [IntPtr]::Zero) {
        throw "The settings HWND did not process WM_NULL within two seconds."
    }

    Write-Output "Overlay smoke test passed: settings remained ${afterWidth}x${afterHeight} and responsive."
    Write-Output "Captured $capturePath"
}
finally {
    if ($primary -and -not $primary.HasExited) {
        Stop-Process -Id $primary.Id
        $primary.WaitForExit()
    }
}
