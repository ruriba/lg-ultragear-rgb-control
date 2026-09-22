# Changelog

## v1.1 (2026-09-22)

### Fixed

- Intermittent state where only the upper-left LED quadrant lit after
  relaunching with image sync on (frame-chunk arming command swallowed while
  the monitor's MCU was busy switching modes).
- Image sync silently disarming when started on a desktop that produces no
  updates (full-screen reader, slideshow): the session now re-arms itself on
  the keepalive cadence until the first frame arrives.
- A latent shader bug where the HDR color path could never activate (the
  constant buffer carrying the format flag was updated every frame but never
  bound).
- Two session-tracking races: a delayed session registration during a USB
  stall could make a freshly started sync session discard all its frames,
  and a stale engine-failure event could kill a newer sync session.
- Audio sync going permanently deaf when the default output device changes
  (speakers → headphones): the loopback now reopens on the new device.

### Changed

- USB write failures now abort the rest of a frame, keeping the report
  stream aligned with the monitor.
- Recovery passes re-arm the monitor only between frames (controlled pause)
  instead of mid-stream.
- Desktop mode changes (HDR toggles) recover in ~1 s instead of several
  seconds of frozen LEDs.
- The GPU readback buffer is created once per session instead of per frame.
- A failed sync start now disarms the monitor instead of leaving it armed at
  full brightness, and quitting keeps an explicitly selected static mode.
- A hung USB write can no longer block quitting or relaunching; HID
  initialization retries instead of disabling all monitor control; the
  autostart entry quotes the executable path (paths with spaces).
- Engine failure reasons are localized, and the status line + tray tooltip
  now reflect the running sync (clearing stale failure text after a
  successful restart).
- The panic log falls back to %TEMP% when the executable's directory is
  read-only.

### Added

- `LGTRAY_STATS=1` diagnostics: frame timings and sync events logged to
  `capture-stats.log` (off by default).

## v1.0 (2026-09-17)

- Initial release.
