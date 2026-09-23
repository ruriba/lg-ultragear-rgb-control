//! USB worker thread: owns the single `HidApi` instance, translates commands
//! to HID writes, discards stale image sync frames by session, and keeps the
//! device present in the background: Windows device-interface notifications
//! (relayed by the UI as [`UsbCommand::Probe`]) wake an immediate presence
//! check whenever an HID device arrives or leaves, with the periodic cadences
//! below as a fallback for missed notifications. Connection changes are
//! published to the UI via the event proxy.
//!
//! Commands and sync frames share one bounded FIFO, and every queued message
//! wakes the worker immediately. Frames are enqueued with try_send: if the
//! writer ever falls behind, the frames it cannot take are dropped instead of
//! piling up, so the backlog is bounded at four frames.

use crate::events::Events;
use crate::usb_protocol::{self, RGBColor};
use hidapi::{HidApi, HidDevice};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// UserEvent id sent to the UI when the connection state changes.
pub const CONNECTION_EVENT: &str = "__connection";

/// UI-only companion of CONNECTION_EVENT for the initial enumeration: it
/// repaints the status line and tooltip, but skips the reconnect recovery
/// (re-pushing saved slots after build_ui's restore burst would drag a
/// resuming sync out of its mode).
pub const CONNECTION_INIT_EVENT: &str = "__connection_init";

const LG_VID: u16 = 0x043E;
/// 0x9A8A: 27GN950/38GN950; 0x9A57: 38GL950G. For the 9A8A the usage page
/// must be 0xFF01 or 0.
const USAGE_PAGE: u16 = 0xFF01;
/// Fallback reconnect attempt cadence while the monitor is missing (a
/// device-list enumeration per attempt). HID arrival notifications normally
/// reconnect instantly; this only backstops them.
const RECONNECT_EVERY: Duration = Duration::from_secs(2);
/// Fallback presence-probe cadence while connected (a device-list
/// enumeration per tick). Device-interface notifications catch unplugs
/// immediately and failing writes catch them during activity, so this only
/// backstops a missed notification while idle.
const PRESENCE_PROBE_EVERY: Duration = Duration::from_secs(120);
/// Consecutive write failures before dropping the device handle.
const MAX_FAILS: u32 = 5;

#[derive(Debug)]
pub enum UsbCommand {
    /// Registers the active image sync session: frames from other sessions are
    /// discarded by the USB thread.
    SetSession(u64),
    TurnOn,
    TurnOff,
    SetBrightness(u8),
    SetMode(u8),
    /// Chunk-count arm command alone (no mode switch): re-asserted after the
    /// arming settle, right before frames resume.
    ArmSync(u8),
    SetStaticColor(u8, u8, u8, u8),
    /// Stores a slot color WITHOUT switching modes: startup restore of the
    /// saved palette (SetStaticColor deliberately switches to the slot).
    StoreStaticColor(u8, u8, u8, u8),
    /// 48-color sync frame; the flag selects the audio (0xC2) marker instead
    /// of video (0xC1).
    SendColors(u64, [RGBColor; 48], bool),
    /// Immediate presence check (a device-interface notification arrived):
    /// the same recovery work as the periodic cadences, without the wait.
    Probe,
    Stop,
}

/// The worker's session bookkeeping, extracted so its ordering guarantees
/// are unit-testable: registration is monotonic (a SetSession parked in
/// Engine::send's fallback thread can never regress the active session and
/// permanently discard a newer session's frames), and only the registered
/// session's frames pass through to the device.
#[derive(Default)]
struct SessionGate {
    active: u64,
}

impl SessionGate {
    fn register(&mut self, s: u64) {
        if s > self.active {
            self.active = s;
        }
    }

    fn accepts(&self, frame_session: u64) -> bool {
        frame_session == self.active
    }
}

pub fn spawn_usb_thread(
    rx: Receiver<UsbCommand>,
    connected: Arc<AtomicBool>,
    events: Events,
) -> JoinHandle<()> {
    thread::Builder::new()
        .name("usb-worker".into())
        .spawn(move || usb_loop(rx, &connected, &events))
        .expect("failed to spawn USB thread")
}

fn usb_loop(rx: Receiver<UsbCommand>, connected: &AtomicBool, events: &Events) {
    // HidApi::new can fail transiently (USB stack still settling at logon).
    // Retrying on the reconnect cadence keeps the worker alive; returning
    // here would silently no-op every menu action for the process lifetime.
    let mut api: Option<HidApi> = HidApi::new().ok();
    if api.is_none() {
        eprintln!("Failed to initialize HidApi; will retry");
    }
    let mut device = api.as_ref().and_then(open_monitor);
    let mut fails = 0u32;
    let mut session = SessionGate::default();
    let mut last_attempt = Instant::now();
    let mut last_probe = Instant::now();
    let found = device.is_some();
    publish_state(connected, events, found, false);
    if found {
        // The initial publish stays silent (the full event would run the
        // reconnect recovery against build_ui's restore burst), but the UI
        // still needs the state: this repaints status + tooltip only.
        events.send(CONNECTION_INIT_EVENT.to_string());
    }

    loop {
        // Every queued message wakes the receive immediately.
        let tick = if device.is_some() {
            PRESENCE_PROBE_EVERY
        } else {
            RECONNECT_EVERY
        };
        let cmd = match rx.recv_timeout(tick) {
            Ok(cmd) => cmd,
            Err(RecvTimeoutError::Timeout) => {
                maintain(
                    &mut api,
                    &mut device,
                    &mut last_attempt,
                    &mut last_probe,
                    connected,
                    events,
                    false,
                );
                continue;
            }
            // All senders dropped (event loop exited): nothing more to do.
            Err(RecvTimeoutError::Disconnected) => break,
        };

        // Stop must be honored even without a device, or the thread would
        // swallow future commands forever.
        if matches!(cmd, UsbCommand::Stop) {
            break;
        }
        // SetSession is pure bookkeeping: process it even without a device —
        // dropping it while the monitor was missing left the gate stale, so
        // after a reconnect every image sync frame was discarded forever.
        // Monotonic: a SetSession parked in Engine::send's fallback thread
        // can land after a newer session's registration; letting it regress
        // the gate would discard the live session's frames forever.
        if let UsbCommand::SetSession(s) = cmd {
            session.register(s);
            continue;
        }

        // A device-interface arrival/removal notification: run the presence
        // check now, bypassing the pacing (notifications are the fast path;
        // the cadences only backstop them).
        if matches!(cmd, UsbCommand::Probe) {
            maintain(
                &mut api,
                &mut device,
                &mut last_attempt,
                &mut last_probe,
                connected,
                events,
                true,
            );
            continue;
        }

        if device.is_none() {
            maintain(
                &mut api,
                &mut device,
                &mut last_attempt,
                &mut last_probe,
                connected,
                events,
                false,
            );
        }
        let Some(dev) = device.as_ref() else {
            continue;
        };
        #[cfg(debug_assertions)]
        {
            let kind = match cmd {
                UsbCommand::SetSession(x) => format!("SetSession({x})"),
                UsbCommand::TurnOn => "TurnOn".into(),
                UsbCommand::TurnOff => "TurnOff".into(),
                UsbCommand::SetBrightness(l) => format!("Brightness({l})"),
                UsbCommand::SetMode(m) => format!("SetMode({m})"),
                UsbCommand::ArmSync(m) => format!("ArmSync({m})"),
                UsbCommand::SetStaticColor(..) => "SetStaticColor".into(),
                UsbCommand::StoreStaticColor(s, r, g, b) => {
                    format!("Store{s}({r:02x}{g:02x}{b:02x})")
                }
                UsbCommand::SendColors(s, _, _) => format!("FRAME s={s}"),
                UsbCommand::Probe => "Probe".into(),
                UsbCommand::Stop => "Stop".into(),
            };
            eprintln!(
                "[usb] {kind} dev={} session={}",
                device.is_some(),
                session.active
            );
        }
        let ok = execute(&cmd, dev, &session);
        on_write(ok, &mut fails, &mut device, connected, events);
    }
}

fn execute(cmd: &UsbCommand, dev: &HidDevice, session: &SessionGate) -> bool {
    match cmd {
        UsbCommand::SendColors(s, colors, audio) => {
            // Frames from a stopped or superseded session: discard.
            if session.accepts(*s) {
                usb_protocol::send_sync_colors(dev, colors, *audio)
            } else {
                true
            }
        }
        UsbCommand::TurnOn => usb_protocol::turn_on(dev),
        UsbCommand::TurnOff => usb_protocol::turn_off(dev),
        UsbCommand::SetBrightness(level) => usb_protocol::set_brightness(dev, *level),
        UsbCommand::SetMode(mode) => usb_protocol::set_mode(dev, *mode),
        UsbCommand::ArmSync(mode) => usb_protocol::arm_sync(dev, *mode),
        UsbCommand::SetStaticColor(slot, r, g, b) => {
            usb_protocol::set_static_color(dev, *slot, *r, *g, *b)
        }
        UsbCommand::StoreStaticColor(slot, r, g, b) => {
            usb_protocol::store_static_color(dev, *slot, *r, *g, *b)
        }
        UsbCommand::SetSession(_) | UsbCommand::Probe | UsbCommand::Stop => {
            // Handled by the drain loop before the device guard. Reachable
            // only if that routing ever changes: ignore rather than panic —
            // a panic here kills the USB worker and with it every monitor
            // action for the rest of the process.
            true
        }
    }
}

fn on_write(
    ok: bool,
    fails: &mut u32,
    device: &mut Option<HidDevice>,
    connected: &AtomicBool,
    events: &Events,
) {
    if ok {
        *fails = 0;
    } else {
        *fails += 1;
        if *fails >= MAX_FAILS {
            // The monitor stopped responding (cable, suspend...): drop the
            // handle; maintain() will refresh and reopen later.
            eprintln!("USB device not responding; reconnecting...");
            *device = None;
            *fails = 0;
            publish_state(connected, events, false, true);
        }
    }
}

/// Attempts a reconnect at most once per `RECONNECT_EVERY`, re-enumerating the
/// device list first (a re-plugged HID device usually gets a new interface
/// path, so stale paths would never match again). While connected, probes the
/// enumeration so an unplug during idle updates the connection state.
/// `force` bypasses the pacing: a device-interface notification arrived and
/// the presence check should run now, not on the next cadence tick.
#[allow(clippy::too_many_arguments)]
fn maintain(
    api: &mut Option<HidApi>,
    device: &mut Option<HidDevice>,
    last_attempt: &mut Instant,
    last_probe: &mut Instant,
    connected: &AtomicBool,
    events: &Events,
    force: bool,
) {
    // A failed HidApi init retries on the same cadence as a reconnect.
    if api.is_none() {
        if !force && last_attempt.elapsed() < RECONNECT_EVERY {
            return;
        }
        *last_attempt = Instant::now();
        *api = HidApi::new().ok();
        if api.is_none() {
            return;
        }
    }
    let api = api.as_mut().expect("api is Some above");
    if device.is_none() {
        if !force && last_attempt.elapsed() < RECONNECT_EVERY {
            return;
        }
        *last_attempt = Instant::now();
        let _ = api.refresh_devices();
        let found = open_monitor(api);
        let is_some = found.is_some();
        *device = found;
        publish_state(connected, events, is_some, true);
        return;
    }
    if force || last_probe.elapsed() >= PRESENCE_PROBE_EVERY {
        *last_probe = Instant::now();
        let _ = api.refresh_devices();
        let still_there = api.device_list().any(matches_monitor);
        if !still_there {
            *device = None;
            publish_state(connected, events, false, true);
        }
    }
}

fn matches_monitor(d: &hidapi::DeviceInfo) -> bool {
    d.vendor_id() == LG_VID
        && match d.product_id() {
            0x9A8A => d.usage_page() == USAGE_PAGE || d.usage_page() == 0,
            0x9A57 => true,
            _ => false,
        }
}

fn open_monitor(api: &HidApi) -> Option<HidDevice> {
    api.device_list()
        .find(|d| matches_monitor(d))
        .and_then(|d| d.open_device(api).ok())
}

fn publish_state(connected: &AtomicBool, events: &Events, is_connected: bool, notify: bool) {
    let prev = connected.swap(is_connected, Ordering::SeqCst);
    // The initial publish must not fire the event: at startup the UI is
    // still building, and its re-push of static slots would race the engine
    // init (the D2 store pulls the monitor out of sync mode).
    if notify && prev != is_connected {
        println!(
            "Monitor {}",
            if is_connected {
                "connected"
            } else {
                "disconnected"
            }
        );
        events.send(CONNECTION_EVENT.to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_registration_is_monotonic() {
        let mut gate = SessionGate::default();
        gate.register(5);
        // A SetSession from an older stop, parked in a fallback thread and
        // landing after a newer registration: must never regress the gate.
        gate.register(3);
        assert_eq!(gate.active, 5);
        gate.register(9);
        assert_eq!(gate.active, 9);
    }

    #[test]
    fn only_the_registered_sessions_frames_pass() {
        let mut gate = SessionGate::default();
        gate.register(7); // a running session
        assert!(gate.accepts(7));
        assert!(!gate.accepts(6)); // a superseded session's late frame
        gate.register(8); // stop's dead token: greater than any live session
        assert!(!gate.accepts(7)); // the stopped session's in-flight frame
        gate.register(9); // the next start
        assert!(gate.accepts(9));
    }
}
