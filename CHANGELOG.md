# Changelog

All notable changes to this project will be documented here. The format follows Keep a Changelog, and releases use Semantic Versioning.

## [Unreleased]

### Added

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

- Extreme image aspect ratios are fitted to the monitor before overlay rasterization.
- Single-instance activation retries startup races and bounds IPC reads by size and time.
- Interactive settings changes are persisted with a short debounce instead of on every pointer event.
- Invalid configured images fall back visibly and reset to the bundled image.
- Windows display geometry refreshes automatically when the monitor topology changes.
