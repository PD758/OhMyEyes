# OhMyEyes

OhMyEyes is a small desktop reminder for the 20-20-20 rule. Every configured interval it displays a translucent eye on the selected monitor, then disappears without interrupting work.

The overlay is always on top, has no background dimming, and is click-through: mouse and keyboard input continue to reach the application underneath it.

## Current status

Version `0.2` supports Windows 10/11 x64 and Wayland compositors implementing `wlr-layer-shell`, including Hyprland and other wlroots compositors, KDE Plasma, and COSMIC.

Shared features:

- Configurable interval, duration, opacity, image width, monitor, and position.
- PNG, JPEG, WebP, animated GIF, and safely rasterized SVG images, with a bundled transparent eye as the default.
- A draggable placement preview.
- Per-user start-at-login, disabled by default.
- Single-instance behavior: launching again opens the existing settings window.

Windows additionally provides:

- An optional tray icon, disabled by default.
- Suspend and session-lock handling with reset or continue behavior.
- An NSIS current-user installer and a portable ZIP release.

The Wayland backend uses a native layer-shell surface on the selected output, an empty input region for click-through behavior, and shared-memory rendering with integer HiDPI scaling. Linux release builds are distributed as a portable tarball. X11 and GNOME Wayland are not supported yet; GNOME requires shell-extension integration.

Linux backends and expanded UI/multi-monitor regression coverage are tracked in [TODO.md](TODO.md).

## Usage

Launch `ohmyeyes.exe` on Windows or `ohmyeyes` on Linux to open settings. Closing settings leaves the timer running in the background. On Linux this performs a short handoff to a headless daemon, so no hidden Wayland toplevel can steal focus or input. Launch the executable again to reopen settings; use `Quit OhMyEyes` in settings or the Windows tray menu to stop it. Use `--background` for a startup without the settings window; OhMyEyes uses this flag for start-at-login. Running a second instance with `--show-now` triggers an immediate reminder and is also useful for overlay testing.

On Windows, settings and logs are stored below `%LOCALAPPDATA%\OhMyEyes`. On Linux, they use the XDG data directory, normally `~/.local/share/OhMyEyes`; autostart uses `~/.config/autostart/ohmyeyes.desktop`. Relative custom-image paths are resolved from the executable directory, which makes them useful in portable installations.

Defaults follow the usual rule: a 20-second reminder every 20 minutes, 55% opacity, 25% of the monitor width, centered on the primary display.

## Development

Windows requirements:

- Rust `1.98.0` (pinned by `rust-toolchain.toml`).
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

Linux requirements:

- Rust `1.98.0`.
- Wayland client development files, `xkbcommon`, EGL, and OpenGL development files.
- A compositor supporting `wlr-layer-shell`.

On Arch Linux and derivatives:

```bash
sudo pacman -S --needed base-devel wayland libxkbcommon mesa
cargo build --release --locked
./target/release/ohmyeyes
```

Create the portable Linux archive with:

```bash
./scripts/package-linux.sh
```

## Architecture

- `config` owns versioned, normalized, atomically persisted settings.
- `scheduler` is a deterministic state machine using monotonic elapsed time.
- `image_asset` validates file size, decoded memory, animation bounds, SVG resources, and supported decoders.
- `app` owns the egui UI, animation timing, and application state.
- `windows` isolates the native layered overlay, IPC, tray, autostart, and OS session notifications.
- `linux_wayland` owns output discovery and the native layer-shell shared-memory overlay.
- `linux_daemon` owns the windowless Linux background scheduler and settings handoff.
- `linux` owns XDG desktop integration; `ipc` provides cross-platform instance activation and a per-user Linux lock.

The root viewport is the normal settings window and receives input directly. On Windows, the reminder is a separate CPU-rendered `WS_EX_LAYERED` window with per-pixel alpha and click-through enabled. This avoids a fullscreen GPU swapchain and lets the settings window remain hidden while the timer runs in the background.

On Wayland, the reminder is a separate `zwlr_layer_surface_v1` in the overlay layer. The surface is image-sized, attached to the selected `wl_output`, non-exclusive, keyboard-inactive, and has an empty pointer input region.

## License

MIT. See [LICENSE](LICENSE).
