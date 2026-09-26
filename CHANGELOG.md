# Changelog

## Unreleased

### Fixed

- Closing the color picker with the window's X made it impossible to open
  again: the X bypassed the picker's teardown, leaving a hidden window
  that every later open silently retargeted instead of showing. The X now
  follows the same path as the Cancel button.

## v1.5.0 (2026-09-26)

### Fixed

- USB commands now travel a dedicated ordered channel that the worker
  always services before sync frames: a manual action can no longer wait
  behind (or be overtaken by) queued frames while the USB writer is busy,
  and stale frames from a stopped sync are dropped before they could
  overwrite a manual change. Shutdown sequences no longer spawn helper
  threads to keep their order. Sync frames themselves now live in a
  latest-wins slot — only the newest colors are ever written to the
  device, so a temporarily busy USB link never replays stale intermediate
  frames.
- Audio Sync now identifies the default output device by its WASAPI
  endpoint ID instead of its friendly name, so a device switch is detected
  even when both devices share a name (or one was renamed).

### Changed

- Settings writes are debounced for rapid changes (the panel's brightness
  slider): one disk write about 0.75 s after the drag ends instead of one
  per step. Discrete actions still save immediately, and anything pending
  is flushed on exit.

## v1.4.0 (2026-09-25)

### Fixed

- The MPO staleness watchdog now backs off when a duplication recreation
  does not restore changing colors (2 → 4 → 8 → 16 s), instead of
  recreating every ~2 s forever on desktops whose sampled-block colors
  never change; a recreation that cures a real freeze resets the backoff
  immediately.

### Added

- Settings UI: a fixed-size window mirroring every tray-menu control
  (modes, brightness, colors, both syncs' tunings, language,
  start-with-Windows), opened from the first menu item or a left click on
  the tray icon and centered on the cursor's monitor. It is a second view
  of the same state, so menu and window can never disagree; closing it
  frees its memory and reopening rebuilds it. Its color pickers are dark
  Slint windows (hue square, hue slider, hex field) replacing the always-
  light ChooseColor dialog. Picking a language switches
  the window, the tray menu and the tooltip immediately. Adds the Slint
  dependency (software renderer, no OpenGL): the exe grows from ~1 MB to
  ~9 MB, and the project license moves from MIT to GPL-3.0 (Slint's
  copyleft option; own code remains the author's to relicense).
- Debug builds open the panel on launch (suppress with
  `LGTRAY_NO_AUTO_PANEL`) and expose a small message-based test hook for
  automated checks.
- Debug builds install a vectored exception handler that logs hard access
  violations (faulting module, accessed address, symbolic backtrace) —
  release builds are unaffected.

## v1.3.0 (2026-09-23)

### Changed

- Audio Sync tuning (sensitivity, color, blink, dynamic range) now applies
  live while the sync runs, instead of restarting the engine (~1 s LED
  pause): the parameters live in a shared slot the loop re-reads every
  cycle, mirroring Image Sync's tuning slot.

## v1.2.0 (2026-09-23)

### Added

- Windows device notifications now wake the USB worker the moment an HID
  device interface arrives or is removed: re-plugging the monitor recovers
  immediately instead of on the next poll. The periodic cadences remain as
  a fallback, and the idle presence probe relaxed from 30 s to 120 s.
- CI attaches the release executable to the GitHub Release when a `v*` tag
  is pushed.

### Changed

- The `windows` dependency tree is unified on 0.62 (the version wasapi
  already pulled), linking one copy of the Win32 bindings instead of two.

### Fixed

- The startup recovery pass could be lost to a race between USB enumeration
  and the UI build at logon autostart, leaving a still-booting lighting MCU
  stuck on its factory preset; the pass is now also scheduled when the first
  enumeration completes (idempotency-guarded).
- Image sync now forces the first post-rearm frame out and restarts the
  keepalive clock, so a monitor that just re-armed no longer sits on its
  factory strip for up to 5 s on a static desktop.
- Quitting during a briefly saturated USB queue could skip the monitor
  disarm restore (brightness left at max on the reverted static preset):
  the restore is now sent with ordered blocking sends.
- The disarm/restore command pairs (static restore after stopping a sync,
  engine-failure disarm, quit) now travel one ordered blocking sequence:
  under a saturated USB queue the two commands could be admitted in either
  order.
- Image sync re-validates its session immediately before enqueueing a frame
  or keepalive: a frame acquired during the stop's up-to-200 ms acquire
  window could previously slip past the session invalidation and land on top
  of the command the user just issued.
- Brightness changes now apply within one capture wakeup while Image Sync
  runs on a still desktop: the software dimming is applied at send time from
  the live level (and the last colors are re-pushed when the level changes)
  instead of being baked into the last computed frame, which a static
  desktop never refreshes.
- settings.json now falls back to `%APPDATA%\lg-ultragear-rgb-control\`
  when the executable's directory is not writable (Program Files installs),
  instead of silently never persisting.

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
