//! USB worker thread: owns the single `HidApi` instance, translates commands
//! to HID writes, discards stale image sync frames by session, and keeps the
//! device present in the background: Windows device-interface notifications
//! (relayed by the UI as [`UsbCommand::Probe`]) wake an immediate presence
//! check whenever an HID device arrives or leaves, with the periodic cadences
//! below as a fallback for missed notifications. Connection changes are
//! published to the UI via the event proxy.
//!
//! Commands travel a two-plane bus ([`command_bus`]):
//!
//! - The **control plane** — everything except sync frames — is a reliable
//!   ordered channel. The worker drains it to exhaustion before servicing
//!   any frame, so a manual command can never be overtaken by a sync frame
//!   nor wait behind a frame backlog, and a stop's `SetSession` invalidates
//!   old frames before they could overwrite what the user just set.
//! - The **data plane** is a single latest-frame slot. Producers publish the
//!   freshest colors under a tiny mutex and offer a wake-up; a newer frame
//!   simply overwrites an unconsumed one, so intermediate colors never
//!   queue up and the worker always writes the newest state there is. The
//!   wake-up is a queued message on a depth-one channel, so it can never be
//!   lost (the frame is stored before the wake-up is offered) and one
//!   pending wake-up covers any number of publishes.

use crate::events::Events;
use crate::usb_protocol::{self, RGBColor};
use hidapi::{HidApi, HidDevice};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, SyncSender, TryRecvError};
use std::sync::{Arc, Mutex};
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

#[derive(Debug, PartialEq)]
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
    /// Immediate presence check (a device-interface notification arrived):
    /// the same recovery work as the periodic cadences, without the wait.
    Probe,
    Stop,
}

/// One sync frame as the data plane carries it: the session it belongs to
/// (checked against the worker's gate before anything reaches the device),
/// the 48 colors, and the audio (0xC2) vs video (0xC1) marker. Both sync
/// sources publish the same shape — at most one engine thread produces at a
/// time, and a frame of the wrong session is gated out anyway.
#[derive(Debug)]
struct SyncFrame {
    session: u64,
    colors: [RGBColor; 48],
    audio: bool,
}

/// The worker's session bookkeeping, extracted so its ordering guarantees
/// are unit-testable: registration is monotonic (a late `SetSession` from a
/// superseded stop can never regress the active session and permanently
/// discard a newer session's frames), and only the registered session's
/// frames pass through to the device.
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

/// The sending half of the USB command bus. Cloned freely (menu, engine
/// threads); every clone sends through the same control channel, wake
/// channel and frame slot.
#[derive(Clone)]
pub struct CommandTx {
    ctrl: Sender<UsbCommand>,
    notify: SyncSender<()>,
    latest: Arc<Mutex<Option<SyncFrame>>>,
}

/// The receiving half, handed once to [`spawn_usb_thread`].
pub struct CommandRx {
    ctrl: Receiver<UsbCommand>,
    notify: Receiver<()>,
    latest: Arc<Mutex<Option<SyncFrame>>>,
}

/// Creates the USB command bus: one control plane and one data plane (see
/// the module docs). The worker owns the receiving halves; the sending half
/// ends up in the `Engine`, which shares it with the menu and its session
/// threads. The wake channel has depth one: a pending wake-up covers every
/// command and frame that arrived before it was consumed, so depth beyond
/// one could only ever hold redundant wake-ups.
pub fn command_bus() -> (CommandTx, CommandRx) {
    let (ctrl_tx, ctrl_rx) = std::sync::mpsc::channel();
    let (notify_tx, notify_rx) = std::sync::mpsc::sync_channel::<()>(1);
    let latest = Arc::new(Mutex::new(None));
    (
        CommandTx {
            ctrl: ctrl_tx,
            notify: notify_tx,
            latest: latest.clone(),
        },
        CommandRx {
            ctrl: ctrl_rx,
            notify: notify_rx,
            latest,
        },
    )
}

impl CommandTx {
    /// Enqueues a control command. Reliable and ordered per channel, and
    /// never blocking: the control plane is unbounded and sized by
    /// user-scale event rates (menu clicks, engine init bursts), so a
    /// command never needs a fallback thread and a wedged writer can never
    /// freeze a sender. Returns false only when the worker is gone.
    pub fn control(&self, cmd: UsbCommand) -> bool {
        let queued = self.ctrl.send(cmd).is_ok();
        // Wake the worker: its only blocking wait is on the wake channel.
        // The offer fails only when a wake-up is already pending — and that
        // one will send the worker straight through the control drain first,
        // so no command can be left waiting behind a dropped offer.
        let _ = self.notify.try_send(());
        queued
    }

    /// Publishes one sync frame to the data plane: latest wins — a frame the
    /// worker has not taken yet is simply overwritten, so only the newest
    /// state is ever written to the device and there is no backlog to drain.
    /// The frame is stored BEFORE the wake-up is offered, and a wake-up is a
    /// queued message (not an edge), so the worker can never sleep through a
    /// published frame. Returns whether an unconsumed frame was superseded
    /// (the engine's "frames coalesced away" statistic).
    pub fn frame(&self, session: u64, colors: [RGBColor; 48], audio: bool) -> bool {
        let mut slot = self.latest.lock().unwrap();
        let superseded = slot.is_some();
        *slot = Some(SyncFrame {
            session,
            colors,
            audio,
        });
        drop(slot);
        let _ = self.notify.try_send(());
        superseded
    }
}

pub fn spawn_usb_thread(
    rx: CommandRx,
    connected: Arc<AtomicBool>,
    events: Events,
) -> JoinHandle<()> {
    thread::Builder::new()
        .name("usb-worker".into())
        .spawn(move || {
            let CommandRx {
                ctrl,
                notify,
                latest,
            } = rx;
            usb_loop(ctrl, notify, latest, &connected, &events)
        })
        .expect("failed to spawn USB thread")
}

/// What [`recv_next`] decided the worker should service.
#[derive(Debug)]
enum Recv {
    /// A control-plane command.
    Ctrl(UsbCommand),
    /// The latest published frame (already taken out of the slot).
    Frame(SyncFrame),
    /// Nothing arrived for one cadence tick: run the periodic maintenance.
    Tick,
    /// Every sender is gone: the process is shutting down.
    Done,
}

/// The worker's receive step — the whole scheduling policy of the bus, kept
/// standalone so its ordering guarantees are unit-testable.
///
/// Per call: drain the control plane first (one command per return, so the
/// worker re-checks it before every frame too), then take whatever the
/// latest-frame slot holds, and only when BOTH are empty block on the wake
/// channel with the maintenance cadence as the timeout. This is the
/// canonical check-predicate-then-wait shape: a producer stores the frame
/// before offering the wake-up, and the wake-up is a queued message — so
/// whichever side of the worker's take the store lands on, either the take
/// sees it or a wake-up is already queued to break the wait. A lost wake-up
/// is structurally impossible, not merely unlikely.
fn recv_next(
    ctrl: &Receiver<UsbCommand>,
    notify: &Receiver<()>,
    latest: &Mutex<Option<SyncFrame>>,
    tick: Duration,
) -> Recv {
    loop {
        match ctrl.try_recv() {
            Ok(cmd) => return Recv::Ctrl(cmd),
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => return Recv::Done,
        }
        if let Some(frame) = latest.lock().unwrap().take() {
            return Recv::Frame(frame);
        }
        match notify.recv_timeout(tick) {
            Ok(()) => continue,
            Err(RecvTimeoutError::Timeout) => return Recv::Tick,
            Err(RecvTimeoutError::Disconnected) => return Recv::Done,
        }
    }
}

fn usb_loop(
    ctrl_rx: Receiver<UsbCommand>,
    notify_rx: Receiver<()>,
    latest: Arc<Mutex<Option<SyncFrame>>>,
    connected: &AtomicBool,
    events: &Events,
) {
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
        let tick = if device.is_some() {
            PRESENCE_PROBE_EVERY
        } else {
            RECONNECT_EVERY
        };
        match recv_next(&ctrl_rx, &notify_rx, &latest, tick) {
            // The latest published frame. Shares the command guards — the
            // reconnect kick when no device is open, the debug trace, the
            // write-failure accounting — but not the routing: the gate at
            // this take is the frame's only referee. The slot is NOT
            // cleared on session registration (that could drop the new
            // session's early frames, published while its own init was
            // still queued); discarding here is the one invalidation
            // mechanism, and a stale frame sitting in the slot costs
            // nothing.
            Recv::Frame(frame) => {
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
                eprintln!(
                    "[usb] FRAME s={} dev={} session={}",
                    frame.session,
                    device.is_some(),
                    session.active
                );
                let ok = if session.accepts(frame.session) {
                    usb_protocol::send_sync_colors(dev, &frame.colors, frame.audio)
                } else {
                    true
                };
                on_write(ok, &mut fails, &mut device, connected, events);
            }
            Recv::Tick => {
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
            Recv::Done => break,

            Recv::Ctrl(cmd) => {
                // Stop must be honored even without a device, or the thread
                // would swallow future commands forever.
                if matches!(cmd, UsbCommand::Stop) {
                    break;
                }
                // SetSession is pure bookkeeping: process it even without a
                // device — dropping it while the monitor was missing left
                // the gate stale, so after a reconnect every image sync
                // frame was discarded forever. Monotonic: a SetSession from
                // a superseded stop can land after a newer session's
                // registration; letting it regress the gate would discard
                // the live session's frames forever.
                if let UsbCommand::SetSession(s) = cmd {
                    session.register(s);
                    continue;
                }

                // A device-interface arrival/removal notification: run the
                // presence check now, bypassing the pacing (notifications
                // are the fast path; the cadences only backstop them).
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
                        UsbCommand::Probe => "Probe".into(),
                        UsbCommand::Stop => "Stop".into(),
                    };
                    eprintln!(
                        "[usb] {kind} dev={} session={}",
                        device.is_some(),
                        session.active
                    );
                }
                let ok = execute(&cmd, dev);
                on_write(ok, &mut fails, &mut device, connected, events);
            }
        }
    }
}

fn execute(cmd: &UsbCommand, dev: &HidDevice) -> bool {
    match cmd {
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
    use std::sync::mpsc::TryRecvError;
    use std::time::Instant;

    fn colors(v: u8) -> [RGBColor; 48] {
        [RGBColor { r: v, g: v, b: v }; 48]
    }

    /// The next control-plane command, asserted by value.
    fn expect_ctrl(ctrl: &Receiver<UsbCommand>, want: &UsbCommand) {
        match ctrl.try_recv() {
            Ok(cmd) => assert_eq!(&cmd, want, "wrong control command"),
            Err(e) => panic!("control plane empty: {e:?}"),
        }
    }

    #[test]
    fn session_registration_is_monotonic() {
        let mut gate = SessionGate::default();
        gate.register(5);
        // A SetSession from an older stop, landing after a newer
        // registration: must never regress the gate.
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

    #[test]
    fn control_commands_keep_order_and_coalesce_wakeups() {
        let (
            tx,
            CommandRx {
                ctrl,
                notify,
                latest,
            },
        ) = command_bus();
        for i in 0..3u8 {
            assert!(tx.control(UsbCommand::SetBrightness(i)));
        }
        for i in 0..3u8 {
            expect_ctrl(&ctrl, &UsbCommand::SetBrightness(i));
        }
        // A burst of control enqueues leaves exactly ONE wake-up pending:
        // the depth-one channel coalesces, and that single pending wake-up
        // already obliges the worker to drain the control plane first.
        assert!(matches!(notify.try_recv(), Ok(())));
        assert!(matches!(notify.try_recv(), Err(TryRecvError::Empty)));
        // A later enqueue offers a fresh wake-up once the channel is free.
        assert!(tx.control(UsbCommand::TurnOff));
        assert!(matches!(notify.try_recv(), Ok(())));
        drop(latest);
    }

    #[test]
    fn latest_frame_wins_and_collapses_backlog() {
        let (
            tx,
            CommandRx {
                ctrl,
                notify,
                latest,
            },
        ) = command_bus();
        // A backlog's worth of publishes without a consumer in sight.
        assert!(!tx.frame(7, colors(1), false)); // nothing superseded yet
        for v in 2..=100u8 {
            assert!(tx.frame(7, colors(v), false)); // each supersedes the last
        }
        // The worker takes exactly ONE frame — the newest — and nothing else.
        match recv_next(&ctrl, &notify, &latest, Duration::from_secs(30)) {
            Recv::Frame(f) => {
                assert_eq!(f.session, 7);
                assert_eq!(f.colors[0].r, 100);
                assert!(!f.audio);
            }
            other => panic!("expected the latest frame, got {other:?}"),
        }
        // No backlog remains: the next receive is a plain cadence tick.
        assert!(matches!(
            recv_next(&ctrl, &notify, &latest, Duration::from_millis(20)),
            Recv::Tick
        ));
    }

    #[test]
    fn control_commands_are_serviced_before_a_pending_frame() {
        let (
            tx,
            CommandRx {
                ctrl,
                notify,
                latest,
            },
        ) = command_bus();
        // A frame is pending in the slot; then the menu's stop + manual
        // sequence arrives.
        tx.frame(7, colors(1), false);
        tx.control(UsbCommand::SetSession(8));
        tx.control(UsbCommand::SetStaticColor(2, 0xAA, 0, 0));
        let long = Duration::from_secs(30);
        match recv_next(&ctrl, &notify, &latest, long) {
            Recv::Ctrl(UsbCommand::SetSession(8)) => {}
            other => panic!("expected SetSession first, got {other:?}"),
        }
        match recv_next(&ctrl, &notify, &latest, long) {
            Recv::Ctrl(UsbCommand::SetStaticColor(2, 0xAA, 0, 0)) => {}
            other => panic!("expected the manual command second, got {other:?}"),
        }
        // Only now is the frame serviced.
        match recv_next(&ctrl, &notify, &latest, long) {
            Recv::Frame(f) => assert_eq!(f.session, 7),
            other => panic!("expected the frame third, got {other:?}"),
        }
    }

    #[test]
    fn control_enqueue_wakes_an_idle_worker() {
        let (
            tx,
            CommandRx {
                ctrl,
                notify,
                latest,
            },
        ) = command_bus();
        let waiter = thread::spawn(move || {
            // The worker's only blocking wait: nothing pending anywhere.
            recv_next(&ctrl, &notify, &latest, Duration::from_secs(30))
        });
        thread::sleep(Duration::from_millis(100)); // let it block
        tx.control(UsbCommand::Probe);
        match waiter.join().unwrap() {
            Recv::Ctrl(UsbCommand::Probe) => {}
            other => panic!("control command did not wake the waiter: {other:?}"),
        }
    }

    #[test]
    fn frame_publish_wakes_an_idle_worker() {
        let (
            tx,
            CommandRx {
                ctrl,
                notify,
                latest,
            },
        ) = command_bus();
        let waiter =
            thread::spawn(move || recv_next(&ctrl, &notify, &latest, Duration::from_secs(30)));
        thread::sleep(Duration::from_millis(100)); // let it block
        tx.frame(9, colors(4), true);
        match waiter.join().unwrap() {
            Recv::Frame(f) => assert_eq!((f.session, f.colors[0].r, f.audio), (9, 4, true)),
            other => panic!("published frame did not wake the waiter: {other:?}"),
        }
    }

    /// The lost-wakeup shape this bus must make impossible: the worker has
    /// drained the control plane, found the slot empty, and is about to
    /// sleep — while the producer is storing a frame. The wake-up is a
    /// QUEUED message and the frame is stored before it is offered, so every
    /// interleaving must return the frame promptly; the 2 s receive tick is
    /// the failure signal, and 1 s of headroom below it is asserted.
    #[test]
    fn wakeup_is_never_lost_against_a_checking_worker() {
        static SWEEP: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        for _ in 0..300 {
            let (
                tx,
                CommandRx {
                    ctrl,
                    notify,
                    latest,
                },
            ) = command_bus();
            let waiter = thread::spawn(move || {
                let started = Instant::now();
                let r = recv_next(&ctrl, &notify, &latest, Duration::from_secs(2));
                (r, started.elapsed())
            });
            // Sweep the publish instant across the worker's check-then-wait
            // transition: before the slot check, inside it, after it.
            let pre_us = SWEEP.fetch_add(1, Ordering::Relaxed) % 300;
            thread::sleep(Duration::from_micros(pre_us as u64));
            tx.frame(9, colors(7), false);
            let (r, dt) = waiter.join().unwrap();
            assert!(
                dt < Duration::from_secs(1),
                "worker stalled {dt:?} — wake-up lost"
            );
            match r {
                Recv::Frame(f) => {
                    assert_eq!((f.session, f.colors[0].r), (9, 7));
                }
                other => panic!("expected the frame, got {other:?}"),
            }
        }
    }

    #[test]
    fn stale_frames_are_discarded_after_a_session_change() {
        let (
            tx,
            CommandRx {
                ctrl,
                notify,
                latest,
            },
        ) = command_bus();
        let mut gate = SessionGate::default();
        gate.register(1); // session 1 is live when the frame is published
        tx.frame(1, colors(9), false);
        // The stop's dead token registers before the worker touches a frame.
        tx.control(UsbCommand::SetSession(2));
        match recv_next(&ctrl, &notify, &latest, Duration::from_secs(30)) {
            Recv::Ctrl(UsbCommand::SetSession(2)) => gate.register(2),
            other => panic!("expected SetSession first, got {other:?}"),
        }
        // The old session's frame is taken — and rejected by the gate, so
        // the worker's write never happens.
        match recv_next(&ctrl, &notify, &latest, Duration::from_secs(30)) {
            Recv::Frame(f) => {
                assert_eq!(f.session, 1);
                assert!(
                    !gate.accepts(f.session),
                    "the stale frame would have been written"
                );
            }
            other => panic!("expected the stale frame, got {other:?}"),
        }
    }

    #[test]
    fn pending_frame_does_not_block_shutdown() {
        let (
            tx,
            CommandRx {
                ctrl,
                notify,
                latest,
            },
        ) = command_bus();
        // A frame sits in the slot and the quit Stop is enqueued when every
        // sender goes away. Queued control commands are still delivered
        // (mpsc drains after disconnect); the pending frame is simply never
        // taken, and the receive ends in Done.
        tx.frame(7, colors(1), false);
        tx.control(UsbCommand::Stop);
        drop(tx);
        match recv_next(&ctrl, &notify, &latest, Duration::from_secs(30)) {
            Recv::Ctrl(UsbCommand::Stop) => {}
            other => panic!("expected Stop, got {other:?}"),
        }
        assert!(matches!(
            recv_next(&ctrl, &notify, &latest, Duration::from_secs(30)),
            Recv::Done
        ));
    }

    #[test]
    fn recv_next_times_out_when_idle() {
        let (
            tx,
            CommandRx {
                ctrl,
                notify,
                latest,
            },
        ) = command_bus();
        assert!(matches!(
            recv_next(&ctrl, &notify, &latest, Duration::from_millis(10)),
            Recv::Tick
        ));
        drop(tx);
    }

    /// The scenario the bus exists for, end to end at the channel level with
    /// the worker's own scheduling step and session gate: a running session
    /// publishing frames, the stop + manual click sequence, and a late
    /// in-flight frame from the dying session (the check→send race the
    /// engine threads can lose during shutdown). The late frame must be
    /// discarded by the gate — it can never execute after the manual
    /// command, whatever the timing.
    #[test]
    fn stopped_sessions_late_frame_never_overtakes_the_manual_command() {
        let (
            tx,
            CommandRx {
                ctrl,
                notify,
                latest,
            },
        ) = command_bus();
        // Session 5 is running and publishing (the slot holds the latest).
        tx.frame(5, colors(1), false);
        tx.frame(5, colors(2), false);
        // The dying engine's late frame: published some time AFTER the stop
        // and the manual command (the worst case of the race).
        let late = tx.clone();
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(25));
            let _ = late.frame(5, colors(3), false);
        });
        // The menu sequence, same thread: stop() then the manual click.
        tx.control(UsbCommand::SetSession(6)); // stop()'s dead token
        tx.control(UsbCommand::SetStaticColor(2, 0xAA, 0, 0));
        drop(tx);

        // A slow USB consumer built from the worker's own pieces: the
        // scheduling step (control plane first), the session gate, and a
        // per-take delay emulating a stalled HID writer.
        let mut gate = SessionGate::default();
        gate.register(5);
        let mut saw_manual = false;
        let mut stale_after_manual = 0;
        loop {
            match recv_next(&ctrl, &notify, &latest, Duration::from_millis(200)) {
                Recv::Ctrl(UsbCommand::SetSession(s)) => gate.register(s),
                Recv::Ctrl(UsbCommand::SetStaticColor(..)) => saw_manual = true,
                Recv::Ctrl(_) => {}
                Recv::Frame(f) => {
                    if gate.accepts(f.session) && saw_manual {
                        stale_after_manual += 1;
                    }
                }
                Recv::Tick | Recv::Done => break,
            }
            thread::sleep(Duration::from_millis(2)); // the slow write
        }
        assert!(saw_manual, "the manual command was never serviced");
        assert_eq!(
            stale_after_manual, 0,
            "a stale frame of the stopped session executed after the manual command"
        );
    }
}
