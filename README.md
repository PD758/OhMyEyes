# OhMyEyes

OhMyEyes is a small desktop reminder for the 20-20-20 rule. Every configured interval it displays a translucent eye on the selected monitor, then disappears without interrupting work.

The overlay is always on top, has no background dimming, and is click-through: mouse and keyboard input continue to reach the application underneath it.

## Current status

Version `0.1` targets Windows 10/11 x64. The core scheduler and configuration are platform-neutral; native Windows integration currently provides:

- Configurable interval, duration, opacity, image width, monitor, and position.
- PNG, JPEG, WebP, animated GIF, and safely rasterized SVG images, with a bundled transparent eye as the default.
- A draggable placement preview.
- Per-user start-at-login, disabled by default.
- An optional tray icon, disabled by default.
- Single-instance behavior: launching again opens the existing settings window.
- Suspend and session-lock handling with reset or continue behavior.
- An NSIS current-user installer and a portable ZIP release.
- SHA-256 checksums for published release files.

Linux support is intentionally not advertised as complete yet. The planned order is Wayland layer-shell support for KDE Plasma, wlroots compositors, and COSMIC, followed by X11. GNOME Wayland requires a shell-extension integration and is tracked separately.

Linux backends and expanded UI/multi-monitor regression coverage are tracked in [TODO.md](TODO.md).

## Usage

Launch `ohmyeyes.exe` to open settings. Closing settings leaves the timer running in the background. Launch the executable again to reopen settings; use `Quit OhMyEyes` in settings or the tray menu to stop it. Use `--background` for a startup without the settings window; OhMyEyes uses this flag for start-at-login. Running a second instance with `--show-now` triggers an immediate reminder and is also used by the overlay regression test.

Settings are stored in `%LOCALAPPDATA%\OhMyEyes\config.json`. Relative custom-image paths are resolved from the executable directory, which makes them useful in portable installations. Logs are stored under `%LOCALAPPDATA%\OhMyEyes\logs`.

Defaults follow the usual rule: a 20-second reminder every 20 minutes, 55% opacity, 25% of the monitor width, centered on the primary display.

## Development

Requirements:

- Rust `1.95.0` (pinned by `rust-toolchain.toml`).
- Visual Studio 2022 Build Tools or Visual Studio with the Desktop development with C++ workload.

On Windows, `build-local.bat` initializes the x64 MSVC environment automatically:

```powershell
.\build-local.bat check
.\build-local.bat test
.\build-local.bat clippy
.\build-local.bat release
.\build-local.bat package
```

To create the portable archive:

```powershell
.\scripts\package.ps1
```

To create the NSIS installer, install [cargo-packager](https://github.com/crabnebula-dev/cargo-packager) and run it from an MSVC developer shell:

```powershell
cargo install cargo-packager --locked --version 0.11.8
.\build-local.bat package
```

## Architecture

- `config` owns versioned, normalized, atomically persisted settings.
- `scheduler` is a deterministic state machine using monotonic elapsed time.
- `image_asset` validates file size, decoded memory, animation bounds, SVG resources, and supported decoders.
- `app` owns the egui UI, animation timing, and application state.
- `windows` isolates the native layered overlay, IPC, tray, autostart, and OS session notifications.

The root viewport is the normal settings window and receives input directly. On Windows, the reminder is a separate CPU-rendered `WS_EX_LAYERED` window with per-pixel alpha and click-through enabled. This avoids a fullscreen GPU swapchain and lets the settings window remain hidden while the timer runs in the background.

## License

MIT. See [LICENSE](LICENSE).
