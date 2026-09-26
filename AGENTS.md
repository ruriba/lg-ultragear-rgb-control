# AGENTS.md

Guidance for coding agents working in this repository. Read before changing
anything: several decisions below look wrong but are load-bearing.

## What this is

System-tray ambilight for LG UltraGear monitors on Windows: Rust, one
portable exe, drives the monitor's 48-LED RGB strip over USB HID. Tray menu
(muda) plus a Slint settings panel. User docs live in README.md and
docs/usage.md; the wire protocol in docs/protocol.md.

## Ground rules

- Repo docs, code comments and commits are in English; conversation with the
  owner happens in Spanish.
- One plain, descriptive commit per unit of work. No AI attribution, no
  co-author trailers. No dev reports or audit write-ups committed to the
  repo; the changelog gets terse user-facing bullets only.
- Never patch or vendor a dependency to shave size — maintenance beats size.
- CI (ci.yml) runs `cargo fmt --check`, `cargo clippy --all-targets --
  -D warnings`, `cargo test`, `cargo build --release`, and attaches
  `target/release/lg-ultragear-rgb-control.exe` to the GitHub Release when a
  `v*` tag is pushed. Bump `version` in Cargo.toml before tagging.

## Building and validating

- `cargo build --release` is the build that ships. The release profile
  (opt-level "z", LTO, one codegen unit, strip, panic=abort) was measured
  CPU-neutral against the default; don't retune it blindly.
- Most behavior needs the real monitor (dev machine has a 38GN950).
  Validation is agent-driven: build small ad-hoc tooling, run the exe, and
  verify outputs yourself (log, USB frames, screenshots); the owner only
  reports what they saw on screen. Repeating a check is fine.
- Debug builds add a crash stack tracer (crash_trace.rs). Frame
  instrumentation for Image Sync appears when the `LGTRAY_STATS` env var is
  set (stats.rs).

## Source map (src/)

Every module opens with a `//!` header that says more than this list.

- `main.rs` — threading model and wiring; read it first.
- `events.rs` — the hidden window + message pump. Every cross-thread
  notification is a queued `String` event (`"mode_3"`, `"toggle_audio"`, …).
- `menu.rs` — tray menu; the single owner of UI state.
- `panel.rs` — Slint panel: a *view* of the menu state, not a second owner.
  Runs on its own thread, which owns the process's only winit event loop.
- `picker.rs` — dark HSV color picker replacing ChooseColor (Windows offers
  no dark variant of that dialog).
- `engine.rs` — Image Sync session: capture → compute → USB, one thread.
- `capture.rs` — DXGI desktop duplication; blocks in-kernel on a still
  desktop (~0% CPU), never poll.
- `compute.rs` + `compute.hlsl` — D3D11 compute shader does the block
  averaging on the GPU; build.rs compiles the HLSL to DXBC at build time.
- `sampling.rs` — pure color math and LED-to-screen geometry.
- `audio.rs` — WASAPI loopback loudness → color, one value per frame.
- `usb.rs` — USB worker thread; owns the single `HidApi` instance, handles
  re-plug detection via device-interface notifications. Commands and frames
  travel separate planes: a reliable ordered control channel the worker
  drains before any frame, and a latest-wins frame slot (see `command_bus`).
- `usb_protocol.rs` — packet format (mirrored in docs/protocol.md).
- `settings.rs` — JSON settings file kept next to the exe. Rapid-fire UI
  changes save through a Win32-timer debounce (events.rs owns the timer;
  menu.rs owns the policy).
- `i18n.rs` — struct-per-language so a missing key breaks the build; live
  language switching rebuilds the menu and panel. `LANGUAGES` starts with
  `System` at index 0 — mind the indexing.

## Hard-won gotchas — do not "fix" these without asking

Win32 / winit:

- winit allows **one event loop per process, forever**. The panel thread
  claims it on first open and never drops it. Panel/picker windows are
  destroy-on-close and re-created; that is what keeps a closed panel at
  ~3 MB RAM (it was 52 MB with a pooled window).
- `WM_PAINT` with an *empty* update region must be ignored: winit's
  `RDW_INTERNALPAINT` feedback loop there once cost 4% CPU while the panel
  was open (fixed → 0.26%). The panel black-window fix is a `WndProc`
  subclass gated on `GetUpdateRect` being non-empty.
- `windows-sys` does not export `PrintWindow`. For window capture, declare
  the extern user32 fn and pass `PW_RENDERFULLCONTENT` (0x2).
- Cross-process pointer injection (`WM_APP` etc. into another process's
  window) is an access violation. Synthetic clicks must go through
  `PostMessage` with a `WM_MOUSEMOVE` first; the tray callback id is
  `WM_USER_TRAYICON` (6002).
- softbuffer: presenting with empty damage must be skipped.
- Docs screenshots: Win11 rounded window corners bleed the desktop through
  PrintWindow — capture with a black desktop background.

Slint:

- Hex color literals are `#RRGGBBAA` (4 bytes), never `#RGB`/`#RRGGBB`.
- Slint cannot decode `.ico`: title-bar icons are set via `WM_SETICON` from
  the embedded resource.
- The software renderer is deliberate (smaller exe, less RAM, no GL driver
  quirks). The OpenGL/femtovg path can be restored by changing the slint
  features in Cargo.toml — only with a reason.
- Panel close is a native hide via HWND; Slint's own hide leaves a blank
  surface on re-show.

Closed decisions (don't reopen without new evidence):

- Tray stays on muda — tray-icon couples through muda's `ContextMenu`
  trait. A hand-rolled `Shell_NotifyIcon` + `TrackPopupMenu` tray is a
  parked v1.5 idea, not a regression.
- The sync watchdog backs off 2→4→8→16 s across DXGI recreation storms and
  resets on a real color change (measured spikes 6.6% → 2.0%).
- Fence-based async GPU readback was evaluated and declined (~0.1–0.2% CPU
  saved); MPO was investigated and left enabled. The optimization thread is
  closed.
