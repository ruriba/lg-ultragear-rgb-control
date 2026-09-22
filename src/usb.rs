//! USB worker thread: owns the single `HidApi` instance, translates commands
//! to HID writes, discards stale image sync frames by session, and keeps trying
//! to reconnect in the background when the monitor disappears (unplug, sleep,
//! re-plug). Connection changes are published to the UI via the event proxy.
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
/// Reconnect attempt cadence while the monitor is missing.
const RECONNECT_EVERY: Duration = Duration::from_secs(2);
/// Presence-probe and idle-tick cadence while connected (a device-list
/// enumeration per tick). An unplug during activity is still detected
/// immediately by failing writes; the probe only matters while idle, where
/// nothing visible depends on it.
const PRESENCE_PROBE_EVERY: Duration = Duration::from_secs(30);
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
    Stop,
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
    let Ok(mut api) = HidApi::new() else {
        eprintln!("Failed to initialize HidApi");
        return;
    };
    let mut device = open_monitor(&api);
    let mut fails = 0u32;
    let mut active_session = 0u64;
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
        // dropping it while the monitor was missing left active_session
        // stale, so after a reconnect every image sync frame was discarded
        // forever.
        if let UsbCommand::SetSession(s) = cmd {
            active_session = s;
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
                UsbCommand::Stop => "Stop".into(),
            };
            eprintln!(
                "[usb] {kind} dev={} session={active_session}",
                device.is_some()
            );
        }
        let ok = execute(&cmd, dev, active_session);
        on_write(ok, &mut fails, &mut device, connected, events);
    }
}

fn execute(cmd: &UsbCommand, dev: &HidDevice, active_session: u64) -> bool {
    match cmd {
        UsbCommand::SendColors(s, colors, audio) => {
            // Frames from a stopped or superseded session: discard.
            if *s == active_session {
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
        UsbCommand::SetSession(_) | UsbCommand::Stop => {
            // Handled by the drain loop before the device guard.
            unreachable!()
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
fn maintain(
    api: &mut HidApi,
    device: &mut Option<HidDevice>,
    last_attempt: &mut Instant,
    last_probe: &mut Instant,
    connected: &AtomicBool,
    events: &Events,
) {
    if device.is_none() {
        if last_attempt.elapsed() < RECONNECT_EVERY {
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
    if last_probe.elapsed() >= PRESENCE_PROBE_EVERY {
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
