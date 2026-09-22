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
/// Grace window after a display-change recreation: a desktop mode
/// transition (HDR toggle, resolution change) keeps even freshly rebuilt
/// duplications delivering frozen frames for several seconds while DWM and
/// the monitor settle (measured: the HDR-enable ramp churns for ~10-20 s),
/// and the watchdog would otherwise recreate through it. Staleness counting
/// pauses for this long after the recreate; the steady-state watchdog is
/// unaffected.
const DISPLAY_CHANGE_SETTLE: Duration = Duration::from_secs(12);

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
    /// Set on a display mode/topology change with an unchanged DeviceName
    /// (e.g. an HDR toggle): the image-sync loop drops its duplication so
    /// the next iteration rebuilds it against the new mode, instead of
    /// waiting out the staleness watchdog (~2 s of frozen LEDs).
    display_changed: AtomicBool,
    /// Set when a recovery pass asks the running session to re-arm the
    /// device: both loops consume it as a controlled reinit (frames pause,
    /// init is re-sent, INIT_SETTLE passes, frames resume). Re-arming
    /// out-of-band while a frame's reports are in flight can leave the
    /// monitor's frame parser misaligned — only the first chunk of each
    /// frame gets applied.
    reinit_requested: AtomicBool,
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
                display_changed: AtomicBool::new(false),
                reinit_requested: AtomicBool::new(false),
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

    /// Asks the running session to re-arm the device through its own
    /// controlled path: frames pause, the init sequence is re-sent, one
    /// INIT_SETTLE passes, frames resume. Used by the recovery passes for
    /// monitor-side power resets the running session never observed (the
    /// lighting MCU boots into its factory state with no HID notification).
    /// The re-arm must never go out-of-band while frames flow: a mode switch
    /// landing between two reports of a frame can leave the monitor's frame
    /// parser applying only the first chunk of every frame.
    pub fn request_reinit(&self) {
        self.state.reinit_requested.store(true, Ordering::SeqCst);
    }

    /// Drops the running session's duplication at the next frame: the
    /// desktop changed mode underneath it (WM_DISPLAYCHANGE with an
    /// unchanged DeviceName — HDR toggle, resolution change) and the
    /// duplication may keep delivering frozen frames in the stale format
    /// until the staleness watchdog would catch it (~2 s of frozen LEDs).
    /// The recreate keeps its 500 ms retry pacing, so this stays safe while
    /// the display transition is still in progress.
    pub fn invalidate_capturer(&self) {
        self.state.display_changed.store(true, Ordering::SeqCst);
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
    // A reinit request left over by a previous session's recovery pass means
    // nothing to this one: the init below re-arms from scratch anyway.
    state.reinit_requested.store(false, Ordering::SeqCst);
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
    // Re-assert the chunk-count arm now that the mode switch has settled:
    // a count swallowed while the MCU was still busy switching is what
    // leaves every sync frame applying only its first chunk. One
    // single-report command, still before the first frame.
    if !state.is_valid(mine) || tx.send(UsbCommand::ArmSync(mode)).is_err() {
        return;
    }

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
    // Watchdog suppression window after a display-change recreation.
    let mut settle_until: Option<Instant> = None;
    // Instrumentation (LGTRAY_STATS): counters for the periodic aggregate
    // line. The adds run unconditionally (immeasurable at 30 Hz); only the
    // stage timers in capture/compute and the file writes are gated.
    let mut stats_log = crate::stats::StatsLog::open();
    stats_log.write_line("t=0s session start image_sync");
    let stats_started = Instant::now();
    let mut stats_flushed = Instant::now();
    let mut arrivals = 0u64;
    let mut none_wakeups = 0u64;
    let mut dedup_skips = 0u64;
    let mut sends = 0u64;
    let mut keepalives = 0u64;
    let mut queue_full = 0u64;
    let mut recreates = 0u64;
    let mut stale_recreates = 0u64;
    let mut srv_sum = 0u64;
    let mut staging_sum = 0u64;
    let mut max_gpu_ms = 0.0f32;
    let mut last_hdr = false;
    // Ring of recent readback Map waits (µs) and GPU dispatch times (ms)
    // for percentiles.
    let mut map_ring = [0u32; 128];
    let mut gpu_ring = [0f32; 128];
    let mut ring_i = 0usize;
    let mut ring_n = 0usize;

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
        // A recovery pass asked for a re-arm: route it through this
        // controlled path (frames pause around the init) instead of ever
        // switching modes while a frame's reports are in flight.
        if state.reinit_requested.swap(false, Ordering::SeqCst) {
            reinit_needed = true;
            stats_log.write_line(&format!(
                "t={}s reinit requested by recovery pass",
                stats_started.elapsed().as_secs()
            ));
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
            // Same reason as run()'s post-settle arm: the chunk count is
            // re-asserted against a settled MCU before frames resume.
            if !state.is_valid(mine) || tx.send(UsbCommand::ArmSync(VIDEO_SYNC_MODE)).is_err() {
                return;
            }
            if !state.is_valid(mine) {
                return;
            }
        }

        // A display mode change with an unchanged DeviceName (HDR toggle,
        // resolution change) leaves the duplication delivering frozen
        // frames; drop it immediately instead of waiting out the staleness
        // watchdog (~2 s). The recreate path below applies its retry pacing.
        if state.display_changed.swap(false, Ordering::SeqCst) {
            capturer = None;
            settle_until = Some(Instant::now() + DISPLAY_CHANGE_SETTLE);
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
                    stats_log.write_line(&format!(
                        "t={}s capture created {w}x{h} hdr={} sampling={:?}",
                        stats_started.elapsed().as_secs(),
                        c.hdr(),
                        params.sampling
                    ));
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
                arrivals += 1;
                if let Some(s) = &fd.stats {
                    map_ring[ring_i] = s.map_wait_us;
                    gpu_ring[ring_i] = s.gpu_ms;
                    ring_i = (ring_i + 1) % map_ring.len();
                    ring_n = (ring_n + 1).min(map_ring.len());
                    if s.gpu_ms > max_gpu_ms {
                        max_gpu_ms = s.gpu_ms;
                    }
                    srv_sum += s.srv_created as u64;
                    staging_sum += s.staging_created as u64;
                    last_hdr = s.hdr;
                }
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
                    let settling = settle_until.is_some_and(|t| Instant::now() < t);
                    if fd.dirty_hit && !settling {
                        stale_frames += 1;
                    } else {
                        stale_frames = 0;
                    }
                } else {
                    stale_frames = 0;
                }
                if stale_frames >= STALE_FRAMES_BEFORE_RECREATE {
                    stale_frames = 0;
                    stale_recreates += 1;
                    eprintln!("Capture stagnant: recreating duplication");
                    recreate = true;
                }
                // Dedup: an unchanged image produces byte-identical frames,
                // and resending them changes nothing for the monitor. The
                // heartbeat after this match keeps it armed during lulls.
                if !same {
                    last_sent = Some(colors);
                    last_send = Instant::now();
                    sends += 1;
                    // try_send: if the USB writer is stalled we drop this
                    // frame instead of ever building a backlog of stale
                    // colors.
                    if let Err(TrySendError::Full(_)) =
                        tx.try_send(UsbCommand::SendColors(mine, colors, false))
                    {
                        queue_full += 1;
                    }
                } else {
                    dedup_skips += 1;
                }
            }
            Ok(None) => none_wakeups += 1,
            Err(CaptureError::AccessLost) => recreate = true,
            Err(CaptureError::ModeChanged) => {
                // The duplication itself reported the new desktop mode:
                // recreate now, and give the transition the same watchdog
                // grace as a display-change recreate — DWM keeps delivering
                // frozen frames for a few seconds while it settles.
                recreate = true;
                settle_until = Some(Instant::now() + DISPLAY_CHANGE_SETTLE);
            }
            Err(e) => {
                eprintln!("Capture error ({e}); recreating...");
                recreate = true;
            }
        }
        if recreate {
            recreates += 1;
            capturer = None;
            continue;
        }

        // Keepalive: the monitor reverts out of sync mode after ~12 s without
        // frames (measured on hardware). Whenever nothing has been sent for
        // SYNC_KEEPALIVE — static desktop, or frames deduped as identical —
        // resend the last computed colors to keep it armed.
        if last_send.elapsed() >= SYNC_KEEPALIVE {
            if let Some(colors) = &last_sent {
                keepalives += 1;
                if let Err(TrySendError::Full(_)) =
                    tx.try_send(UsbCommand::SendColors(mine, *colors, false))
                {
                    queue_full += 1;
                }
            }
            last_send = Instant::now();
        }

        // Frame pacing: cap USB sends at the configured fps. DXGI conflates
        // desktop updates while we sleep, so the next acquire always returns
        // the freshest frame — no backlog, no stale colors.
        if let Some(rest) = frame_ms.checked_sub(start.elapsed()) {
            thread::sleep(rest);
        }

        // Periodic aggregate line for the LGTRAY_STATS log (release builds
        // have no console, so this is the only observable trace).
        if stats_log.is_open() && stats_flushed.elapsed() >= Duration::from_secs(5) {
            stats_flushed = Instant::now();
            let elapsed = stats_started.elapsed().as_secs().max(1);
            let (p50, p95, map_max) = map_percentiles(&map_ring, ring_n);
            let gpu_p50 = gpu_percentile(&gpu_ring, ring_n);
            let frames = arrivals.max(1) as f64;
            stats_log.write_line(&format!(
                "t={elapsed}s fps={:.1} wakeups={:.1}/s dedup={:.1}% send={:.1}/s \
                 ka={keepalives} qfull={queue_full} map[p50={p50}us p95={p95}us \
                 max={map_max}us] gpu[p50={gpu_p50:.3}ms max={max_gpu_ms:.3}ms] \
                 srv={:.2}/f stg={:.2}/f \
                 rec={recreates} stale={stale_recreates} hdr={} mode={:?} cap={}",
                arrivals as f64 / elapsed as f64,
                none_wakeups as f64 / elapsed as f64,
                if arrivals > 0 {
                    dedup_skips as f64 / arrivals as f64 * 100.0
                } else {
                    0.0
                },
                sends as f64 / elapsed as f64,
                srv_sum as f64 / frames,
                staging_sum as f64 / frames,
                last_hdr,
                params.sampling,
                params.fps,
            ));
        }
    }
}

/// p50/p95/max over the valid prefix of the map-wait ring buffer.
fn map_percentiles(ring: &[u32; 128], n: usize) -> (u32, u32, u32) {
    if n == 0 {
        return (0, 0, 0);
    }
    let mut sorted = ring[..n].to_vec();
    sorted.sort_unstable();
    let at = |p: usize| sorted[(n - 1) * p / 100];
    (at(50), at(95), sorted[n - 1])
}

/// p50 of the GPU dispatch times, ignoring NaN (failed query) samples.
fn gpu_percentile(ring: &[f32; 128], n: usize) -> f32 {
    let mut valid: Vec<f32> = ring[..n].iter().filter(|v| !v.is_nan()).copied().collect();
    if valid.is_empty() {
        return f32::NAN;
    }
    valid.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    valid[valid.len() * 50 / 100]
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
        // Same recovery-pass contract as the image loop: re-arm through the
        // controlled pause, never mid-stream.
        if state.reinit_requested.swap(false, Ordering::SeqCst) {
            reinit_needed = true;
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
            // Same reason as run()'s post-settle arm: the chunk count is
            // re-asserted against a settled MCU before frames resume.
            if !state.is_valid(mine) || tx.send(UsbCommand::ArmSync(AUDIO_SYNC_MODE)).is_err() {
                return;
            }
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
