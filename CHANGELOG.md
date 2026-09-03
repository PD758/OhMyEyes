# Changelog

All notable changes to this project will be documented here. The format follows Keep a Changelog, and releases use Semantic Versioning.

## [Unreleased]

### Added

- Native Wayland `wlr-layer-shell` overlay support for Hyprland/wlroots, KDE Plasma, and COSMIC.
- Wayland output selection, hot-plug refresh, integer HiDPI rendering, and empty input regions for click-through reminders.
- Linux single-instance activation and XDG autostart integration.
- A headless Linux background daemon with daemon-to-settings handoff, avoiding hidden Wayland toplevels.
- Portable Linux release archives and Linux CI coverage.
- Initial Windows 10/11 implementation.
- Configurable native Windows layered overlay with per-pixel alpha and no fullscreen GPU surface.
- Settings persistence, custom images, display selection, and draggable placement.
- Optional tray icon, per-user autostart, and single-instance activation.
- Sleep and session-lock scheduling policies.
- Windows CI, NSIS installer metadata, and portable packaging.
- SHA-256 checksum manifests for release artifacts.
- Animated GIF images with bounded frame count, decoded memory, and cycle duration.
- SVG images with bounded rasterization and external-resource loading disabled.

### Changed

- Linux instance and IPC endpoints are scoped per user, use a close-on-exec file lock, and tolerate the bounded listener teardown race during process handoff.
- Production image loading is fallible instead of relying on `expect`, Linux forbids unsafe code, and every Win32 unsafe block documents its safety invariant.
- Extreme image aspect ratios are fitted to the monitor before overlay rasterization.
- The single-instance IPC endpoint is bound before configuration and image loading.
- Windows lock notifications retry while Remote Desktop Services starts and report failures in settings.
- Animated overlays reuse image-sized CPU and GDI buffers instead of rebuilding monitor-sized surfaces.
- Image and configuration reads enforce their limits on a single open file handle.
- Windows display selection uses the DisplayConfig monitor device path with legacy ID migration.
- Interactive settings changes are persisted with a short debounce instead of on every pointer event.
- Invalid configured images fall back visibly and reset to the bundled image.
- Windows display geometry refreshes automatically when the monitor topology changes.
