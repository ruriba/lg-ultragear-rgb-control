//! Image Sync engine: one session mechanism, one USB channel, one source (the
//! sampled screen). At most one engine thread runs at any time; manual menu
//! commands always take precedence over frames.

use crate::audio::{self, AudioColor, Blink, DynamicRange};
use crate::capture::{CaptureError, Capturer};
use crate::events::Events;
use crate::sampling::{self, SamplingMode};
use crate::usb::UsbCommand;
use crate::usb_protocol::RGBColor;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::mpsc::{SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use windows::Win32::System::Threading::{
    GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_BELOW_NORMAL,
};

/// Device mode fed by image sync.
pub const VIDEO_SYNC_MODE: u8 = 8;
/// Device mode fed by audio sync.
pub const AUDIO_SYNC_MODE: u8 = 7;
/// Brightness the sync modes run at.
const SYNC_BRIGHTNESS: u8 = 12;

/// Blocking acquire window. Bounds both the wakeup rate on a static desktop
/// (~5/s, zero work per wakeup) and the session-stop reaction latency.
const ACQUIRE_TIMEOUT_MS: u32 = 200;
/// Pacing for capture (re)creation attempts: avoids ACCESS_LOST storms while a
/// mode change or fullscreen transition is in progress. Not applied to a
/// session's first creation, which has no previous duplication to collide
/// with.
const RETRY_DELAY: Duration = Duration::from_millis(500);
/// Heartbeat for static scenes: DXGI stops presenting on an unchanged
/// desktop, and the monitor reverts out of sync mode after ~12 s without
/// frames (measured), falling back to its last static preset. Resending the
/// last computed colors at this cadence keeps it armed.
const SYNC_KEEPALIVE: Duration = Duration::from_secs(5);
/// Idle period while the monitor is absent: the USB worker would discard every
/// frame anyway, so capture and sampling pause entirely until it is back.
/// Nothing is visible during absence, so a full second between wakeups is
/// free; resuming from stillness just adds this much to the reconnect.
const DISCONNECTED_IDLE: Duration = Duration::from_secs(1);
/// Settling time after the init/re-init sequence, before frames resume. The
/// monitor needs a real gap between the mode-8 arming burst and the first
/// sync frame: with ~100 ms it stays in its previous static mode (measured).
/// 250 ms arms reliably while keeping startup near-instant.
const INIT_SETTLE: Duration = Duration::from_millis(250);
/// MPO staleness watchdog: with hardware overlay planes active, the
/// duplication can keep delivering frames with frozen pixels while the real
/// desktop moves on. Recreating the duplication restores live content.
/// Signal: this many consecutive ARRIVED frames with identical sampled
/// colors (~2 s at 30 fps). A genuinely static desktop delivers no frames at
/// all, so it never triggers this.
const STALE_FRAMES_BEFORE_RECREATE: u32 = 60;

/// UserEvent prefix carrying the reason an engine gave up on its own: the UI
/// unchecks the sync toggle and surfaces the reason in the status item.
pub const ENGINE_FAILED_EVENT: &str = "__engine_failed;";

/// What the engine should produce.
pub enum Source {
    /// Screen sampling (device video-sync mode). Tuning is read live from
    /// the engine's shared params slot, so preset changes apply without
    /// restart.
    ImageSync { screen: String },
    /// Loudness-reactive gradient (device audio-sync mode).
    Audio {
        gain: f32,
        color: AudioColor,
        blink: Blink,
        range: DynamicRange,
    },
}

/// Image Sync tuning, carried from the menu to the sampler.
#[derive(Clone, Copy, PartialEq)]
pub struct ImageSyncParams {
    /// Which part of the screen the LEDs sample.
    pub sampling: SamplingMode,
    /// 0.0 = instant reaction, up to ~0.9 = heavy blending.
    pub smoothing: f32,
    /// Color multiplier to project more vivid light.
    pub boost: f32,
    /// Target frames per second (USB send cap).
    pub fps: u32,
}

impl Default for ImageSyncParams {
    fn default() -> Self {
        Self {
            sampling: SamplingMode::Border5,
            smoothing: 0.4,
            boost: 1.2,
            fps: 30,
        }
    }
}

/// Shared engine state: `running` plus a monotonically increasing session id.
/// A session is valid only while `running` is set and `session` still equals
/// the session's own token — bumping `session` retires every old thread.
pub struct EngineState {
    running: AtomicBool,
    session: AtomicU64,
}

impl EngineState {
    fn is_valid(&self, mine: u64) -> bool {
        self.running.load(Ordering::SeqCst) && self.session.load(Ordering::SeqCst) == mine
    }
}

pub struct Engine {
    state: Arc<EngineState>,
    tx: SyncSender<UsbCommand>,
    image_params: Arc<Mutex<ImageSyncParams>>,
    /// Brightness level 1..=12, applied by the engine as software dimming.
    brightness: Arc<AtomicU8>,
    /// Monitor HID presence, published by the USB worker. The engine idles
    /// while it is false instead of producing frames the worker would discard.
    connected: Arc<AtomicBool>,
    events: Events,
}

impl Engine {
    pub fn new(tx: SyncSender<UsbCommand>, connected: Arc<AtomicBool>, events: Events) -> Self {
        Self {
            state: Arc::new(EngineState {
                running: AtomicBool::new(false),
                session: AtomicU64::new(0),
            }),
            tx,
            image_params: Arc::new(Mutex::new(ImageSyncParams::default())),
            brightness: Arc::new(AtomicU8::new(SYNC_BRIGHTNESS)),
            connected,
            events,
        }
    }

    /// Live-updates image sync tuning: the running loop reads the slot every
    /// frame, so only sampling-mode changes (which need new capture regions)
    /// still require a restart.
    pub fn set_image_params(&self, params: ImageSyncParams) {
        *self.image_params.lock().unwrap() = params;
    }

    /// Brightness level 1..=12. During sync it is applied as software
    /// dimming of the frames (the device sits at max), so changing it never
    /// stops a running sync; on static modes it is sent to the device.
    pub fn set_brightness(&self, level: u8) {
        self.brightness
            .store(level.clamp(1, SYNC_BRIGHTNESS), Ordering::Relaxed);
    }

    /// Non-blocking send for callers on the UI thread: a wedged USB writer
    /// can keep the FIFO full for seconds and the tray must never freeze on
    /// it. A full FIFO hands the command to a transient thread instead.
    pub fn send(&self, cmd: UsbCommand) {
        if let Err(TrySendError::Full(cmd)) = self.tx.try_send(cmd) {
            let tx = self.tx.clone();
            thread::spawn(move || {
                let _ = tx.send(cmd);
            });
        }
    }

    /// Starts the given sync source, unless the engine is already running.
    /// The session token leaves any previous thread (from before a stop and
    /// quick restart) out of the game.
    pub fn start(&self, source: Source) {
        if self.state.running.swap(true, Ordering::SeqCst) {
            return;
        }
        let mine = self.state.session.fetch_add(1, Ordering::SeqCst) + 1;
        let state = self.state.clone();
        let tx = self.tx.clone();
        let image_params = self.image_params.clone();
        let brightness = self.brightness.clone();
        let connected = self.connected.clone();
        let events = self.events;
        thread::spawn(move || {
            run(
                source,
                tx,
                image_params,
                brightness,
                events,
                connected,
                state,
                mine,
            )
        });
    }

    /// Stops the engine and invalidates the session IN THE USB THREAD TOO.
    /// `SetSession(dead)` travels through the same FIFO channel the menu uses,
    /// so it lands before any manual command enqueued afterwards; any late
    /// `SendColors` from the old session is then discarded by the USB thread.
    /// (Without this, one in-flight frame could overwrite the user's command.)
    pub fn stop(&self) {
        // dead > any live session: the next start gets dead+1
        let dead = self.state.session.fetch_add(1, Ordering::SeqCst) + 1;
        self.send(UsbCommand::SetSession(dead));
        self.state.running.store(false, Ordering::SeqCst);
    }

    /// Re-sends the session init minus SetSession (still active): on, sync
    /// brightness, armed mode. Idempotent re-assertion for monitor-side power
    /// resets the running session never observed: the lighting MCU boots into
    /// its factory state (Static 4, blue) with no HID notification, and when
    /// the USB enumeration survives the outage nothing else re-arms it.
    pub fn reassert_sync(&self, mode: u8) {
        for cmd in reinit_commands(mode) {
            self.send(cmd);
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn run(
    source: Source,
    tx: SyncSender<UsbCommand>,
    image_params: Arc<Mutex<ImageSyncParams>>,
    brightness: Arc<AtomicU8>,
    events: Events,
    connected: Arc<AtomicBool>,
    state: Arc<EngineState>,
    mine: u64,
) {
    // Below-normal priority: a demanding scene must never fight the UI for CPU.
    unsafe {
        let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_BELOW_NORMAL);
    }

    // Validate BEFORE sending anything: a session that was superseded at birth
    // must not stomp a manual command with its init sequence.
    if !state.is_valid(mine) {
        return;
    }
    // Init sequence: on, max brightness, then the device mode the source
    // feeds. Re-validated before every send so nothing lands after the
    // user's command or the quit restore.
    let mode = match &source {
        Source::ImageSync { .. } => VIDEO_SYNC_MODE,
        Source::Audio { .. } => AUDIO_SYNC_MODE,
    };
    match &source {
        Source::ImageSync { .. } => {
            let p = image_params.lock().unwrap();
            println!(
                "Engine started (image sync, {} @ {} fps, smoothing {:.1}, boost {:.1})",
                match p.sampling {
                    SamplingMode::Border5 => "border 5%",
                    SamplingMode::Border15 => "border 15%",
                    SamplingMode::Full => "full screen",
                },
                p.fps,
                p.smoothing,
                p.boost
            );
        }
        Source::Audio {
            gain,
            color,
            blink,
            range,
        } => {
            println!(
                "Engine started (audio sync, gain {:.1}, color {:?}, blink {:?}, range {:?})",
                gain, color, blink, range
            );
        }
    }
    let alive = [
        UsbCommand::SetSession(mine),
        UsbCommand::TurnOn,
        UsbCommand::SetBrightness(SYNC_BRIGHTNESS),
        UsbCommand::SetMode(mode),
    ];
    for cmd in alive {
        if !state.is_valid(mine) || tx.send(cmd).is_err() {
            return;
        }
    }
    // Process the init sequence now instead of waiting for the first frame,
    // then give the monitor the settle it needs to arm the sync mode: frames
    // sent before arming are ignored and the monitor stays in its previous
    // static mode.
    if !state.is_valid(mine) {
        return;
    }
    thread::sleep(INIT_SETTLE);

    match source {
        Source::ImageSync { screen } => image_sync_loop(
            &screen,
            &image_params,
            &brightness,
            &tx,
            &events,
            &connected,
            &state,
            mine,
        ),
        Source::Audio {
            gain,
            color,
            blink,
            range,
        } => audio_loop(
            gain,
            color,
            blink,
            range,
            &tx,
            &brightness,
            &events,
            &connected,
            &state,
            mine,
        ),
    }
    println!("Engine stopped");
}

/// Commands re-sent after a reconnect: the session init minus SetSession
/// (still active). The USB worker discarded the original along with the
/// frames while the monitor was absent.
fn reinit_commands(mode: u8) -> [UsbCommand; 3] {
    [
        UsbCommand::TurnOn,
        UsbCommand::SetBrightness(SYNC_BRIGHTNESS),
        UsbCommand::SetMode(mode),
    ]
}

#[allow(clippy::too_many_arguments)]
fn image_sync_loop(
    screen: &str,
    image_params: &Mutex<ImageSyncParams>,
    brightness: &AtomicU8,
    tx: &SyncSender<UsbCommand>,
    events: &Events,
    connected: &AtomicBool,
    state: &EngineState,
    mine: u64,
) {
    let mut prev = [[0u8; 3]; 48];
    // First frame skips temporal smoothing, so the LEDs snap to the real
    // colors instead of fading in from black.
    let mut first_frame = true;
    // Set when an iteration observes the monitor absent: this session's init
    // sequence (on/brightness/mode) is discarded by the USB worker together
    // with the frames while the device is gone, so it must be re-sent on
    // reconnect or the first frames land in whatever mode the device booted in.
    let mut reinit_needed = false;
    let mut capturer: Option<Capturer> = None;
    // Retry counter for rate-limited console output. A missing output would
    // otherwise print every RETRY_DELAY forever.
    let mut create_fails = 0u32;
    // False only before a session's very first capture creation: the
    // RETRY_DELAY pacing exists to separate consecutive duplications.
    let mut first_creation = true;
    // Dedup + keepalive state: last colors the monitor received, and when.
    let mut last_sent: Option<[RGBColor; 48]> = None;
    let mut last_send = Instant::now();
    // Consecutive arrived frames with identical colors (staleness watchdog).
    let mut stale_frames = 0u32;

    loop {
        if !state.is_valid(mine) {
            break;
        }

        // Read tuning live from the shared slot: preset changes apply on the
        // next frame with no restart. The frame period follows the configured
        // fps: the cap keeps the worker light; higher rates just trade CPU
        // for snappier LEDs.
        let params = *image_params.lock().unwrap();
        let frame_ms = Duration::from_millis((1000 / params.fps.max(1) as u64).max(1));

        // Monitor absent (unplug, sleep, KVM switch): pause capture entirely
        // instead of burning CPU on frames nobody will consume.
        if !connected.load(Ordering::SeqCst) {
            reinit_needed = true;
            thread::sleep(DISCONNECTED_IDLE);
            continue;
        }
        if reinit_needed {
            // Re-arm the device (idempotent commands); the settle mirrors
            // run()'s init sequence.
            for cmd in reinit_commands(VIDEO_SYNC_MODE) {
                if !state.is_valid(mine) || tx.send(cmd).is_err() {
                    return;
                }
            }
            reinit_needed = false;
            thread::sleep(INIT_SETTLE);
            if !state.is_valid(mine) {
                return;
            }
        }

        // (Re)create the duplication. Format and blocks are re-derived on
        // every (re)creation: a Windows HDR toggle keeps the resolution but
        // changes the pixel format, so caching by resolution would corrupt
        // colors.
        if capturer.is_none() {
            let retry = !first_creation;
            first_creation = false;
            if retry {
                thread::sleep(RETRY_DELAY);
            }
            if !state.is_valid(mine) {
                break;
            }
            match Capturer::new(screen, params.sampling) {
                Ok(c) => {
                    let (w, h) = c.dimensions();
                    println!("Capturing {}x{}{}", w, h, if c.hdr() { " HDR" } else { "" });
                    // The desktop came back after an outage (standby, input
                    // switch): the monitor sat unarmed through its ~12 s sync
                    // timeout, so re-arm before the first frame lands or the
                    // frames would be ignored.
                    if create_fails > 0 {
                        reinit_needed = true;
                    }
                    create_fails = 0;
                    capturer = Some(c);
                }
                Err(CaptureError::Gpu(msg)) => {
                    // Terminal: this machine cannot run Image Sync at all.
                    eprintln!("Image sync: {msg}; stopping");
                    events.send(format!(
                        "{ENGINE_FAILED_EVENT}Image sync unavailable: {msg}"
                    ));
                    state.running.store(false, Ordering::SeqCst);
                    return;
                }
                Err(e) => {
                    create_fails += 1;
                    if create_fails == 1 || create_fails.is_multiple_of(10) {
                        eprintln!("Capture init failed ({e}); retrying (attempt {create_fails})");
                    }
                    continue;
                }
            }
        }

        let start = Instant::now();
        let mut recreate = false;
        match capturer.as_mut().unwrap().frame(ACQUIRE_TIMEOUT_MS) {
            Ok(Some(fd)) => {
                let mut colors = sampling::finalize(
                    &fd.sums,
                    &fd.counts,
                    &mut prev,
                    &mut first_frame,
                    params.smoothing,
                    params.boost,
                );
                // Software dimming: the device sits at max brightness during
                // sync and the level is applied here, so changing it never
                // stops the sync.
                let lvl = brightness.load(Ordering::Relaxed);
                if lvl < SYNC_BRIGHTNESS {
                    for c in colors.iter_mut() {
                        c.r = (c.r as u32 * lvl as u32 / SYNC_BRIGHTNESS as u32) as u8;
                        c.g = (c.g as u32 * lvl as u32 / SYNC_BRIGHTNESS as u32) as u8;
                        c.b = (c.b as u32 * lvl as u32 / SYNC_BRIGHTNESS as u32) as u8;
                    }
                }
                let same = last_sent.as_ref() == Some(&colors);
                // MPO staleness watchdog: frames keep arriving with frozen
                // pixels while the real desktop moves on (hardware overlay
                // planes bypass the duplicated image). Recreating the
                // duplication restores live content; a genuinely static
                // desktop delivers no frames, so it never fires there.
                // Identical colors alone are NOT enough to count the frame:
                // desktop activity outside the sampled blocks leaves them
                // untouched while the duplication is perfectly healthy (this
                // used to recreate a live session every few seconds). Only
                // frames whose dirty rects overlap the sampled area count.
                if same {
                    if fd.dirty_hit {
                        stale_frames += 1;
                    } else {
                        stale_frames = 0;
                    }
                } else {
                    stale_frames = 0;
                }
                if stale_frames >= STALE_FRAMES_BEFORE_RECREATE {
                    stale_frames = 0;
                    eprintln!("Capture stagnant: recreating duplication");
                    recreate = true;
                }
                // Dedup: an unchanged image produces byte-identical frames,
                // and resending them changes nothing for the monitor. The
                // heartbeat after this match keeps it armed during lulls.
                if !same {
                    last_sent = Some(colors);
                    last_send = Instant::now();
                    // try_send: if the USB writer is stalled we drop this
                    // frame instead of ever building a backlog of stale
                    // colors.
                    let _ = tx.try_send(UsbCommand::SendColors(mine, colors, false));
                }
            }
            Ok(None) => {}
            Err(CaptureError::AccessLost) => recreate = true,
            Err(e) => {
                eprintln!("Capture error ({e}); recreating...");
                recreate = true;
            }
        }
        if recreate {
            capturer = None;
            continue;
        }

        // Keepalive: the monitor reverts out of sync mode after ~12 s without
        // frames (measured on hardware). Whenever nothing has been sent for
        // SYNC_KEEPALIVE — static desktop, or frames deduped as identical —
        // resend the last computed colors to keep it armed.
        if last_send.elapsed() >= SYNC_KEEPALIVE {
            if let Some(colors) = &last_sent {
                let _ = tx.try_send(UsbCommand::SendColors(mine, *colors, false));
            }
            last_send = Instant::now();
        }

        // Frame pacing: cap USB sends at the configured fps. DXGI conflates
        // desktop updates while we sleep, so the next acquire always returns
        // the freshest frame — no backlog, no stale colors.
        if let Some(rest) = frame_ms.checked_sub(start.elapsed()) {
            thread::sleep(rest);
        }
    }
}

/// as 0xC2 audio-sync frames, with the base palette (rainbow sweep or one
/// solid color) scaled every frame by the smoothed loudness of the system
/// audio (the monitor has no DSP of its own — LG's original software feeds it
/// exactly like this from the PC side).
#[allow(clippy::too_many_arguments)] // the Source::Audio fields + loop plumbing
fn audio_loop(
    gain: f32,
    color: AudioColor,
    blink: Blink,
    range: DynamicRange,
    tx: &SyncSender<UsbCommand>,
    brightness: &AtomicU8,
    events: &Events,
    connected: &AtomicBool,
    state: &EngineState,
    mine: u64,
) {
    let base: [RGBColor; 48] = match color {
        AudioColor::Rainbow => {
            std::array::from_fn(|i| sampling::hsl_to_rgb(i as f32 / 48.0, 1.0, 0.5))
        }
        AudioColor::Solid(rgb) => {
            [RGBColor {
                r: rgb[0],
                g: rgb[1],
                b: rgb[2],
            }; 48]
        }
    };
    let mut mic = match audio::LoopbackLoudness::open() {
        Ok(mic) => mic,
        Err(e) => {
            eprintln!("Audio sync: no usable loopback capture ({e}); stopping");
            events.send(format!(
                "{ENGINE_FAILED_EVENT}Audio sync: audio capture unavailable ({e})"
            ));
            // Clear the running flag so a retry from the menu isn't swallowed.
            state.running.store(false, Ordering::SeqCst);
            return;
        }
    };
    // Consecutive capture failures tolerated before reopening the loopback: a
    // re-plug, driver restart or default-device change invalidates the old
    // endpoint, and without this the LEDs froze forever at the last level.
    // ~3.3 s of tolerance absorbs transient glitches without flapping.
    const MAX_CAPTURE_FAILURES: u32 = 100;
    let mut failures = 0u32;
    // Level to paint while the endpoint is failing (kept from before, so
    // short glitches cause no visible change).
    let mut last_level = 0.0f32;
    // Same reconnect contract as the image loop: init is discarded along with
    // the frames while the monitor is absent.
    let mut reinit_needed = false;
    // Dedup + keepalive state: last colors the monitor received, and when.
    let mut last_sent: Option<[RGBColor; 48]> = None;
    let mut last_send = Instant::now();
    loop {
        if !state.is_valid(mine) {
            break;
        }
        // Monitor absent: skip painting (the USB worker would discard the
        // frames). The loopback is left undrained — at most 200 ms of audio
        // buffers up device-side, and reconnect resumes from fresh sound.
        if !connected.load(Ordering::SeqCst) {
            reinit_needed = true;
            thread::sleep(DISCONNECTED_IDLE);
            continue;
        }
        if reinit_needed {
            for cmd in reinit_commands(AUDIO_SYNC_MODE) {
                if !state.is_valid(mine) || tx.send(cmd).is_err() {
                    return;
                }
            }
            reinit_needed = false;
            // The monitor was just re-armed: force the next frame out even
            // if it matches what went before the outage.
            last_sent = None;
            thread::sleep(INIT_SETTLE);
            if !state.is_valid(mine) {
                return;
            }
        }
        // Software dimming: the device sits at max and the brightness level
        // scales the loudness level here, so changing it never stops the
        // sync.
        let dim = brightness.load(Ordering::Relaxed) as f32 / SYNC_BRIGHTNESS as f32;
        let level = match mic.next_level(gain, blink, range) {
            Some(level) => {
                failures = 0;
                level * dim
            }
            None => {
                failures += 1;
                if failures >= MAX_CAPTURE_FAILURES {
                    eprintln!("Audio sync: capture failed; reopening loopback");
                    match audio::LoopbackLoudness::open() {
                        Ok(fresh) => {
                            mic = fresh;
                            failures = 0;
                            last_level = 0.0;
                        }
                        Err(e) => {
                            eprintln!("Audio sync: reopen failed ({e}); stopping");
                            events.send(format!(
                                "{ENGINE_FAILED_EVENT}Audio sync: audio capture unavailable ({e})"
                            ));
                            // Clear the running flag so a retry from the menu
                            // isn't swallowed.
                            state.running.store(false, Ordering::SeqCst);
                            return;
                        }
                    }
                }
                last_level * dim
            }
        };
        if !state.is_valid(mine) {
            break;
        }
        let colors = std::array::from_fn(|i| {
            let g = base[i];
            RGBColor {
                r: (g.r as f32 * level) as u8,
                g: (g.g as f32 * level) as u8,
                b: (g.b as f32 * level) as u8,
            }
        });
        // Dedup: silence converges the envelope to a byte-identical frame,
        // and resending it changes nothing on the monitor. The heartbeat
        // below keeps the sync mode armed during lulls (same mechanism as
        // the image loop).
        if last_sent.as_ref() != Some(&colors) {
            last_sent = Some(colors);
            last_send = Instant::now();
            let _ = tx.try_send(UsbCommand::SendColors(mine, colors, true));
        }
        // Keepalive: the monitor reverts out of sync mode after ~12 s without
        // frames (measured on hardware). Whenever nothing has been sent for
        // SYNC_KEEPALIVE — silence, or a failing endpoint — resend the last
        // frame to keep it armed.
        if last_send.elapsed() >= SYNC_KEEPALIVE {
            if let Some(colors) = &last_sent {
                let _ = tx.try_send(UsbCommand::SendColors(mine, *colors, true));
            }
            last_send = Instant::now();
        }
    }
}
