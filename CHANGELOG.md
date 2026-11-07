# Changelog

All notable changes to this project are documented here.
The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project
adheres to [Semantic Versioning](https://semver.org/).

## [Unreleased]

## [0.1.0] - 2026-11-07

First public version.

### Added
- Floating capsule dock for Hyprland, rendered with OpenGL ES 3 on a `wlr-layer-shell` surface.
- "Liquid Glass" shader: lensed bevel that refracts a snapshot of the backdrop, chromatic
  dispersion, specular rim lights, frost, adaptive tint for bright backdrops and an analytic shadow.
- macOS-style parabolic magnification, name pill, launch bounce and running-app indicators.
- Auto-hide with a hot zone at the bottom edge, a 0.45 s hide delay and zero CPU while idle.
- Right-click context menu drawn by the dock: new window, desktop actions, minimize, close,
  move left/right, pin/unpin. `+` button to pin more apps (rofi picker).
- One surface per monitor in a single process, hot-plug aware, fractional-scale aware
  (`wp-fractional-scale` + `wp-viewporter`).
- Backdrop capture through `wlr-screencopy`; the Intel/Mesa GPU is forced for EGL so a discrete
  NVIDIA GPU can stay asleep.
- Pinned apps stored in `~/.config/glassdock/pinned.json`, with a generic first-run seed.
- English and Spanish UI strings (picked from the locale).
- `glassdock-launch` (supervisor), `glassdockctl` (control socket) and an installer/uninstaller.
