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
//! - The **data plane** is a tiny bounded frame queue. Producers offer
//!   frames with try_send and frames the USB writer cannot take are simply
//!   dropped — only the freshest colors matter, so there is never a backlog
//!   of stale frames to drain.

use crate::events::Events;
use crate::usb_protocol::{self, RGBColor};
use hidapi::{HidApi, HidDevice};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, SyncSender, TryRecvError};
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
/// Depth of the bounded frame queue. Four frames is ~130 ms of headroom at
/// 30 fps against a transiently busy writer; beyond that the frames are
/// stale anyway (the next one carries fresher colors), so they are dropped
/// instead of queued.
const FRAME_QUEUE: usize = 4;

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
    /// 48-color sync frame; the flag selects the audio (0xC2) marker instead
    /// of video (0xC1).
    SendColors(u64, [RGBColor; 48], bool),
    /// Immediate presence check (a device-interface notification arrived):
    /// the same recovery work as the periodic cadences, without the wait.
    Probe,
    Stop,
    /// Data-plane-only wake token: control-plane enqueues push one through
    /// the frame channel so a worker blocked there (its only blocking wait)
    /// drains the command immediately. Never executed; consumed by
    /// [`recv_next`].
    Wake,
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
/// threads); every clone sends through the same two channels.
#[derive(Clone)]
pub struct CommandTx {
    ctrl: Sender<UsbCommand>,
    frames: SyncSender<UsbCommand>,
}

/// The receiving half, handed once to [`spawn_usb_thread`].
pub struct CommandRx {
    ctrl: Receiver<UsbCommand>,
    frames: Receiver<UsbCommand>,
}

/// Creates the USB command bus: one control plane and one data plane (see
/// the module docs). The worker owns the receiving halves; the sending half
/// ends up in the `Engine`, which shares it with the menu and its session
/// threads.
pub fn command_bus() -> (CommandTx, CommandRx) {
    let (ctrl_tx, ctrl_rx) = std::sync::mpsc::channel();
    let (frame_tx, frame_rx) = std::sync::mpsc::sync_channel(FRAME_QUEUE);
    (
        CommandTx {
            ctrl: ctrl_tx,
            frames: frame_tx,
        },
        CommandRx {
            ctrl: ctrl_rx,
            frames: frame_rx,
        },
    )
}

impl CommandTx {
    /// Enqueues a control command. Reliable and ordered per channel, and
    /// never blocking: the control plane is unbounded and sized by
    /// user-scale event rates (menu clicks, engine init bursts), so a
    /// command never needs a fallback thread and a wedged writer can never
    /// freeze a sender. Frames handed here by mistake route to the lossy
    /// data plane. Returns false only when the worker is gone.
    pub fn control(&self, cmd: UsbCommand) -> bool {
        match cmd {
            UsbCommand::SendColors(session, colors, audio) => {
                self.frame(session, colors, audio);
                true
            }
            cmd => {
                let queued = self.ctrl.send(cmd).is_ok();
                // Wake the worker: its only blocking wait is on the frame
                // channel, so every control enqueue pokes it through there.
                // The poke fails only when the frame queue is full — and
                // then the worker is demonstrably busy draining frames, and
                // drains the control plane before servicing the next frame
                // anyway. No poke, no lost wakeup, in every combination.
                let _ = self.frames.try_send(UsbCommand::Wake);
                queued
            }
        }
    }

    /// Offers one sync frame to the data plane: dropped when the USB writer
    /// is backed up (a stale frame is worthless — the next one carries
    /// fresher colors, so only the backlog is bounded, never the latency of
    /// what follows). Returns whether the frame was queued.
    pub fn frame(&self, session: u64, colors: [RGBColor; 48], audio: bool) -> bool {
        self.frames
            .try_send(UsbCommand::SendColors(session, colors, audio))
            .is_ok()
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
            let CommandRx { ctrl, frames } = rx;
            usb_loop(ctrl, frames, &connected, &events)
        })
        .expect("failed to spawn USB thread")
}

/// What [`recv_next`] decided the worker should service.
#[derive(Debug)]
enum Recv {
    /// A command from either plane.
    Cmd(UsbCommand),
    /// Nothing arrived for one cadence tick: run the periodic maintenance.
    Tick,
    /// Every sender is gone: the process is shutting down.
    Done,
}

/// The worker's receive step — the whole scheduling policy of the bus, kept
/// standalone so its ordering guarantees are unit-testable.
///
/// The control plane is drained to exhaustion first: anything enqueued there
/// (a stop's `SetSession`, a manual command, the quit `Stop`) is serviced
/// before any frame, however saturated the frame queue is. Only when the
/// control plane is empty does the worker take one message off the frame
/// channel — a frame to execute, or a `Wake` left by a control enqueue,
/// which just loops back into the drain. The blocking wait carries the
/// maintenance cadence as its timeout, exactly like the worker's single
/// channel did before the split.
fn recv_next(ctrl: &Receiver<UsbCommand>, frames: &Receiver<UsbCommand>, tick: Duration) -> Recv {
    loop {
        match ctrl.try_recv() {
            Ok(cmd) => return Recv::Cmd(cmd),
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => return Recv::Done,
        }
        match frames.recv_timeout(tick) {
            Ok(UsbCommand::Wake) => continue,
            Ok(cmd) => return Recv::Cmd(cmd),
            Err(RecvTimeoutError::Timeout) => return Recv::Tick,
            Err(RecvTimeoutError::Disconnected) => return Recv::Done,
        }
    }
}

fn usb_loop(
    ctrl_rx: Receiver<UsbCommand>,
    frame_rx: Receiver<UsbCommand>,
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
        let cmd = match recv_next(&ctrl_rx, &frame_rx, tick) {
            Recv::Cmd(cmd) => cmd,
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
                continue;
            }
            Recv::Done => break,
        };

        // Stop must be honored even without a device, or the thread would
        // swallow future commands forever.
        if matches!(cmd, UsbCommand::Stop) {
            break;
        }
        // SetSession is pure bookkeeping: process it even without a device —
        // dropping it while the monitor was missing left the gate stale, so
        // after a reconnect every image sync frame was discarded forever.
        // Monotonic: a SetSession from a superseded stop can land after a
        // newer session's registration; letting it regress the gate would
        // discard the live session's frames forever.
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
                UsbCommand::Wake => "Wake".into(),
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
        UsbCommand::SetSession(_) | UsbCommand::Probe | UsbCommand::Stop | UsbCommand::Wake => {
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

    fn colors(v: u8) -> [RGBColor; 48] {
        [RGBColor { r: v, g: v, b: v }; 48]
    }

    /// The next control-plane command, asserted by pattern.
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
    fn control_commands_keep_order_and_poke_the_frame_channel() {
        let (tx, CommandRx { ctrl, frames }) = command_bus();
        // A short burst (pokes beyond the frame-queue capacity are dropped by
        // design — the worker is then demonstrably not blocked — so keep the
        // burst inside it).
        for i in 0..3u8 {
            assert!(tx.control(UsbCommand::SetBrightness(i)));
        }
        for i in 0..3u8 {
            expect_ctrl(&ctrl, &UsbCommand::SetBrightness(i));
        }
        // Each control enqueue left exactly one Wake on the frame channel.
        for _ in 0..3 {
            assert!(matches!(frames.try_recv(), Ok(UsbCommand::Wake)));
        }
        assert!(matches!(frames.try_recv(), Err(TryRecvError::Empty)));
    }

    #[test]
    fn frames_are_lossy_when_the_queue_is_full() {
        let (tx, CommandRx { ctrl, frames }) = command_bus();
        for _ in 0..FRAME_QUEUE {
            assert!(tx.frame(7, colors(1), false));
        }
        // Saturated: the next frame is dropped, never blocking the producer.
        assert!(!tx.frame(7, colors(2), false));
        // And control commands still go through untouched.
        assert!(tx.control(UsbCommand::TurnOff));
        drop(tx);
        drop(ctrl);
        drop(frames);
    }

    #[test]
    fn sendcolors_routed_through_control_takes_the_lossy_path() {
        let (tx, CommandRx { ctrl, frames }) = command_bus();
        assert!(tx.control(UsbCommand::SendColors(3, colors(9), true)));
        assert!(matches!(ctrl.try_recv(), Err(TryRecvError::Empty)));
        match frames.try_recv() {
            Ok(UsbCommand::SendColors(s, c, a)) => {
                assert_eq!((s, c[0].r, a), (3, 9, true));
            }
            other => panic!("frame missing from the data plane: {other:?}"),
        }
    }

    #[test]
    fn control_plane_is_drained_before_a_saturated_frame_queue() {
        let (tx, CommandRx { ctrl, frames }) = command_bus();
        for _ in 0..FRAME_QUEUE {
            assert!(tx.frame(7, colors(1), false));
        }
        assert!(!tx.frame(7, colors(2), false)); // saturated
                                                 // The menu's stop + manual sequence, enqueued while saturated.
        tx.control(UsbCommand::SetSession(8));
        tx.control(UsbCommand::SetStaticColor(2, 0xAA, 0, 0));
        let long = Duration::from_secs(120);
        // Both commands are serviced before ANY frame, in order.
        match recv_next(&ctrl, &frames, long) {
            Recv::Cmd(UsbCommand::SetSession(8)) => {}
            other => panic!("expected SetSession first, got {other:?}"),
        }
        match recv_next(&ctrl, &frames, long) {
            Recv::Cmd(UsbCommand::SetStaticColor(2, 0xAA, 0, 0)) => {}
            other => panic!("expected the manual command second, got {other:?}"),
        }
        // Only now does a frame come through.
        match recv_next(&ctrl, &frames, long) {
            Recv::Cmd(UsbCommand::SendColors(..)) => {}
            other => panic!("expected a frame, got {other:?}"),
        }
    }

    #[test]
    fn control_enqueue_wakes_a_worker_blocked_on_frames() {
        let (tx, CommandRx { ctrl, frames }) = command_bus();
        let waiter = thread::spawn(move || {
            // The worker's only blocking wait: nothing on either channel.
            recv_next(&ctrl, &frames, Duration::from_secs(30))
        });
        thread::sleep(Duration::from_millis(100)); // let it block
        tx.control(UsbCommand::Probe);
        match waiter.join().unwrap() {
            Recv::Cmd(UsbCommand::Probe) => {}
            other => panic!("control command did not wake the waiter: {other:?}"),
        }
    }

    #[test]
    fn recv_next_times_out_and_finishes_on_disconnect() {
        let (tx, CommandRx { ctrl, frames }) = command_bus();
        assert!(matches!(
            recv_next(&ctrl, &frames, Duration::from_millis(10)),
            Recv::Tick
        ));
        drop(tx);
        assert!(matches!(
            recv_next(&ctrl, &frames, Duration::from_millis(10)),
            Recv::Done
        ));
    }

    /// The scenario the bus exists for, end to end at the channel level with
    /// the worker's own scheduling step and session gate: a saturated frame
    /// queue of a running session, the stop + manual click sequence, and a
    /// late in-flight frame from the dying session (the check→send race the
    /// engine threads can lose during shutdown). The late frame must be
    /// discarded by the gate — it can never execute after the manual
    /// command, whatever the timing.
    #[test]
    fn stopped_sessions_late_frame_never_overtakes_the_manual_command() {
        let (tx, CommandRx { ctrl, frames }) = command_bus();
        // Session 5 is running and has saturated the frame queue.
        for _ in 0..FRAME_QUEUE {
            assert!(tx.frame(5, colors(1), false));
        }
        // The dying engine's in-flight frame: enqueued some time AFTER the
        // stop and the manual command (the worst case of the race).
        let late = tx.clone();
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(25));
            let _ = late.frame(5, colors(2), false);
        });
        // The menu sequence, same thread: stop() then the manual click.
        tx.control(UsbCommand::SetSession(6)); // stop()'s dead token
        tx.control(UsbCommand::SetStaticColor(2, 0xAA, 0, 0));
        drop(tx);

        // A slow USB consumer built from the worker's own pieces: the
        // scheduling step (control plane first), the session gate, and a
        // per-write delay emulating a stalled HID writer.
        let mut gate = SessionGate::default();
        gate.register(5);
        let mut saw_manual = false;
        let mut stale_after_manual = 0;
        loop {
            match recv_next(&ctrl, &frames, Duration::from_millis(200)) {
                Recv::Cmd(UsbCommand::SetSession(s)) => gate.register(s),
                Recv::Cmd(UsbCommand::SendColors(s, _, _)) => {
                    if gate.accepts(s) && saw_manual {
                        stale_after_manual += 1;
                    }
                }
                Recv::Cmd(UsbCommand::SetStaticColor(..)) => saw_manual = true,
                Recv::Cmd(_) => {}
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
