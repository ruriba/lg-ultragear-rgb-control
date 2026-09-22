//! Optional frame instrumentation for Image Sync, active only when the
//! `LGTRAY_STATS` environment variable is set (value irrelevant). Zero cost
//! otherwise: every per-frame write lives behind [`FrameStats`], which the
//! pipeline only builds when [`enabled`] is true, and the stage timers never
//! call `Instant::now` when disabled.
//!
//! The release binary is a windows-subsystem app (stdout goes nowhere), so
//! output lands in `capture-stats.log` next to the executable — the same
//! resolution as the panic log — falling back to `%TEMP%`. One file per
//! process run, truncated at open: a benchmark run is one log, and the
//! aggregate lines carry their own elapsed-time stamps.

use std::fs::File;
use std::io::Write;
use std::sync::OnceLock;
use std::time::Instant;

/// True for the whole process lifetime when stats were requested.
pub fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("LGTRAY_STATS").is_some())
}

/// Per-frame pipeline stage timings, filled by capture.rs and compute.rs.
/// Durations are microseconds (u32 saturates at ~71 minutes; every stage is
/// microseconds to low milliseconds). Written only on frames that arrived
/// with new desktop content.
#[derive(Default)]
pub struct FrameStats {
    /// `AcquireNextFrame` wall time for an arrived frame (kernel wait + DDA
    /// bookkeeping). Timeout wakeups are counted by the engine, not here.
    pub acquire_us: u32,
    /// Dirty-rect query + sampled-block overlap test.
    pub dirty_us: u32,
    /// `CreateShaderResourceView` on the duplication frame texture alone.
    pub srv_create_us: u32,
    /// Rest of the GPU submit: cbuffer update, binds, Dispatch, unbinds.
    pub submit_us: u32,
    /// The blocking readback `Map` — the pipeline's only CPU↔GPU fence.
    pub map_wait_us: u32,
    /// GPU dispatch duration from timestamp queries; NaN when the query
    /// failed or came back not-ready.
    pub gpu_ms: f32,
    /// Resource creations on this frame (the SRV is the only one left; the
    /// staging readback buffer became session-persistent in v1.1).
    pub srv_created: u8,
    /// Per-frame pixel format (MPO setups can flip it mid-session).
    pub hdr: bool,
}

/// Stage timer: measures nothing (and costs nothing) unless stats are on.
/// `Stage::start(stats.is_some())` at the top of a section, one `us()` read
/// at the end.
pub struct Stage(Option<Instant>);

impl Stage {
    pub fn start(on: bool) -> Self {
        Self(on.then(Instant::now))
    }

    /// Microseconds elapsed since `start` (0 when stats are off).
    pub fn us(&self) -> u32 {
        self.0
            .map(|t| t.elapsed().as_micros().min(u32::MAX as u128) as u32)
            .unwrap_or(0)
    }
}

/// Append-only stats file. Inert (no file) when stats are disabled or the
/// log cannot be created.
pub struct StatsLog {
    file: Option<File>,
}

impl StatsLog {
    /// Opens (truncating) `capture-stats.log` next to the executable when
    /// stats are enabled; falls back to `%TEMP%` when that directory is not
    /// writable.
    pub fn open() -> Self {
        let file = if enabled() {
            let primary = std::env::current_exe()
                .ok()
                .and_then(|p| p.parent().map(|d| d.join("capture-stats.log")));
            primary
                .and_then(|path| File::create(path).ok())
                .or_else(|| {
                    File::create(std::env::temp_dir().join("lg-ultragear-capture-stats.log")).ok()
                })
        } else {
            None
        };
        Self { file }
    }

    pub fn is_open(&self) -> bool {
        self.file.is_some()
    }

    /// Writes one line immediately (session markers and aggregate flushes;
    /// the caller owns the cadence).
    pub fn write_line(&mut self, line: &str) {
        if let Some(f) = self.file.as_mut() {
            let _ = writeln!(f, "{line}");
            let _ = f.flush();
        }
    }
}
