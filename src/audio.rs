//! WASAPI loopback capture of whatever Windows is playing, reduced to one
//! smoothed loudness value per frame. The monitor has no audio DSP: in the
//! original LG software the PC analyzes the sound and streams 0xC2 frames
//! while the device sits in audio mode — this module is that missing half.

use std::collections::VecDeque;
use std::time::Duration;
use wasapi::{
    initialize_mta, AudioCaptureClient, AudioClient, DeviceEnumerator, Direction, StreamMode,
    WaveFormat,
};

/// How often loudness is sampled (~30 fps, like video sync).
const POLL_EVERY: Duration = Duration::from_millis(33);
/// Loopback buffer in 100 ns units: 200 ms, generous slack against overrun.
const BUFFER_HNS: i64 = 2_000_000;
/// How often the default render device's identity is re-checked (~1 s of
/// polls): a device switch leaves the old loopback capturing that old
/// device's silence without ever failing.
const DEVICE_CHECK_POLLS: u32 = 30;

/// What the audio sync paints: the rainbow sweep or one solid color.
#[derive(Clone, Copy, PartialEq, Eq, Debug, serde::Serialize, serde::Deserialize)]
pub enum AudioColor {
    Rainbow,
    Solid([u8; 3]),
}

/// Temporal envelope of the loudness response: how fast the level climbs on
/// a beat (attack) and how long it falls afterwards (release).
#[derive(Clone, Copy, PartialEq, Eq, Debug, serde::Serialize, serde::Deserialize)]
pub enum Blink {
    Smooth,
    Normal,
    Fast,
}

impl Blink {
    /// (attack, release) blending factors applied per frame.
    fn envelope(self) -> (f32, f32) {
        match self {
            Blink::Smooth => (0.35, 0.04),
            Blink::Normal => (1.0, 0.15),
            Blink::Fast => (1.0, 0.55),
        }
    }
}

/// How loudness maps onto 0..1: compression lifts quiet content, expansion
/// leaves only the peaks visible, extreme turns the strip into beat strobes.
#[derive(Clone, Copy, PartialEq, Eq, Debug, serde::Serialize, serde::Deserialize)]
pub enum DynamicRange {
    Compressed,
    Normal,
    Expanded,
    Extreme,
}

impl DynamicRange {
    /// (scale, gamma) of the loudness -> level curve. Exaggerated ranges keep
    /// a HIGH scale so peaks still saturate to full brightness: the gamma
    /// suppresses the quiet parts, not the peaks.
    fn curve(self) -> (f32, f32) {
        match self {
            DynamicRange::Compressed => (3.5, 0.6),
            DynamicRange::Normal => (2.4, 0.9),
            DynamicRange::Expanded => (2.0, 1.5),
            DynamicRange::Extreme => (3.0, 2.2),
        }
    }
}

pub struct LoopbackLoudness {
    audio_client: AudioClient,
    capture_client: AudioCaptureClient,
    queue: VecDeque<u8>,
    /// Envelope state (fast attack, slow release) in 0..1.
    level: f32,
    /// Friendly name of the render device the loopback was opened on.
    device_name: String,
    polls_since_check: u32,
}

impl LoopbackLoudness {
    /// Opens loopback capture of the default render device. Fails without a
    /// usable output device.
    pub fn open() -> Result<Self, String> {
        initialize_mta()
            .ok()
            .map_err(|e| format!("COM init: {e}"))?;
        let enumerator = DeviceEnumerator::new().map_err(|e| e.to_string())?;
        let device = enumerator
            .get_default_device(&Direction::Render)
            .map_err(|e| e.to_string())?;
        let device_name = device.get_friendlyname().map_err(|e| e.to_string())?;
        let mut audio_client = device.get_iaudioclient().map_err(|e| e.to_string())?;
        let format: WaveFormat = audio_client.get_mixformat().map_err(|e| e.to_string())?;
        // Capture direction on a render device = loopback: the crate sets
        // AUDCLNT_STREAMFLAGS_LOOPBACK for this combination.
        audio_client
            .initialize_client(
                &format,
                &Direction::Capture,
                &StreamMode::PollingShared {
                    autoconvert: true,
                    buffer_duration_hns: BUFFER_HNS,
                },
            )
            .map_err(|e| e.to_string())?;
        let capture_client = audio_client
            .get_audiocaptureclient()
            .map_err(|e| e.to_string())?;
        audio_client.start_stream().map_err(|e| e.to_string())?;
        Ok(Self {
            audio_client,
            capture_client,
            queue: VecDeque::new(),
            level: 0.0,
            device_name,
            polls_since_check: 0,
        })
    }

    /// True when the default render device's friendly name differs from the
    /// one this loopback was opened on: the old capture then keeps
    /// delivering that device's silence without ever failing, and only a
    /// reopen picks up the new output. Errors read as "unchanged" — a real
    /// device loss surfaces through the capture failures instead.
    fn default_device_changed(&self) -> bool {
        let Ok(enumerator) = DeviceEnumerator::new() else {
            return false;
        };
        let Ok(device) = enumerator.get_default_device(&Direction::Render) else {
            return false;
        };
        device
            .get_friendlyname()
            .map(|name| name != self.device_name)
            .unwrap_or(false)
    }

    /// Waits one frame period, drains everything played since the last call
    /// and returns the smoothed loudness in 0..1, or `None` when the capture
    /// read failed (device invalidated: unplug, driver restart, default-device
    /// change). On failure the caller keeps painting the last level and
    /// reopens the loopback after a run of consecutive failures.
    pub fn next_level(&mut self, gain: f32, blink: Blink, range: DynamicRange) -> Option<f32> {
        std::thread::sleep(POLL_EVERY);
        self.polls_since_check += 1;
        if self.polls_since_check >= DEVICE_CHECK_POLLS {
            self.polls_since_check = 0;
            if self.default_device_changed() {
                return None;
            }
        }
        // Drain every packet available: read_from_device_to_deque fetches a
        // single WASAPI packet per call, and a 33 ms poll that ate one ~10 ms
        // packet would fall permanently behind in the 200 ms buffer.
        // GetNextPacketSize reports 0 frames (Some(0)) when the queue is
        // empty — None only happens in exclusive mode.
        loop {
            match self.capture_client.get_next_packet_size() {
                Ok(Some(n)) if n > 0 => {
                    if self
                        .capture_client
                        .read_from_device_to_deque(&mut self.queue)
                        .is_err()
                    {
                        return None;
                    }
                }
                Ok(_) => break,
                Err(_) => return None,
            }
        }
        let samples = self.queue.make_contiguous();
        let mut acc = 0.0f64;
        let mut n = 0usize;
        for chunk in samples.as_chunks::<4>().0 {
            let s = f32::from_le_bytes(*chunk) as f64;
            acc += s * s;
            n += 1;
        }
        self.queue.clear();
        if n > 0 {
            let rms = (acc / n as f64).sqrt() as f32;
            // Music RMS sits around 0.05..0.3: the range curve maps it into
            // 0..1, then fast attack / slow release so beats bump instead of
            // strobing.
            let (scale, gamma) = range.curve();
            let (attack, release) = blink.envelope();
            let target = (rms * scale * gain).clamp(0.0, 1.0).powf(gamma);
            self.level = if target > self.level {
                self.level * (1.0 - attack) + target * attack
            } else {
                self.level * (1.0 - release) + target * release
            };
        }
        Some(self.level)
    }
}

impl Drop for LoopbackLoudness {
    fn drop(&mut self) {
        let _ = self.audio_client.stop_stream();
    }
}
