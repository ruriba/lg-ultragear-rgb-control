//! LG UltraGear RGB tray control.
//!
//! Threading model:
//! - main thread: Win32 message pump (blocked in GetMessage, 0% CPU) + tray
//!   menu. `events.rs` owns the hidden window that receives menu ids, USB
//!   connection changes, display-topology changes and engine failures as
//!   queued String events.
//! - USB worker: owns the HidApi instance, reconnects in the background
//! - engine thread (0..1): image sync capture; frames go to
//!   the USB worker through one bounded FIFO channel shared with commands.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod audio;
mod capture;
mod color_dialog;
mod compute;
mod engine;
mod events;
mod i18n;
mod menu;
mod sampling;
mod settings;
mod usb;
mod usb_protocol;

use engine::Engine;
use menu::{build_menu, build_ui, handle_event};
use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use tray_icon::menu::MenuEvent;
use tray_icon::TrayIconBuilder;
use usb::UsbCommand;

fn main() {
    install_panic_hook();
    if !acquire_single_instance_lock() {
        return;
    }

    // i18n::init must run before any menu string is built.
    let settings = settings::load();
    i18n::init(settings.language.unwrap_or(i18n::Language::System));

    // Hidden window receiving every event (menu clicks included) before the
    // tray exists: Shell_NotifyIcon needs no running message loop, so the
    // tray is created right away and early events simply queue up.
    let events = events::init().expect("failed to create message window");

    // One bounded FIFO for commands AND frames: manual commands may block
    // briefly on send, frames always use try_send and get dropped when the
    // USB writer is backed up. Deep enough (8) that the startup restore
    // burst (brightness + 4 slots) never needs a transient-thread send,
    // which would race the engine's init for FIFO order.
    let (tx, rx) = mpsc::sync_channel::<UsbCommand>(8);
    let connected = Arc::new(AtomicBool::new(false));
    let engine = Arc::new(Engine::new(tx.clone(), connected.clone(), events));
    let usb_handle = usb::spawn_usb_thread(rx, connected.clone(), events);

    let mut ui = build_ui(engine, connected, settings, events);
    let menu = build_menu(&ui);

    // Menu events arrive through muda's global handler on this thread; they
    // join the same event queue as the cross-thread notifications.
    MenuEvent::set_event_handler(Some(move |ev: MenuEvent| {
        events.send(ev.id.as_ref().to_string());
    }));

    let state = if ui.connected.load(Ordering::SeqCst) {
        i18n::t().connected
    } else {
        i18n::t().disconnected
    };
    let tray = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip(format!("LG UltraGear RGB Control — {state}"))
        .with_icon(build_icon())
        .build()
        .expect("failed to create tray icon");
    ui.tray = Some(tray);

    // Returns when handle_event sets quit (or the loop dies).
    events::pump(&mut ui, handle_event);

    // Bounded: ~1 s per queued write at worst, so this join cannot hang.
    let _ = usb_handle.join();
}

fn build_icon() -> tray_icon::Icon {
    // Same icon embedded by build.rs as the .exe resource (assets/app.ico, ID 1).
    tray_icon::Icon::from_resource(1, None).expect("missing embedded app icon")
}

/// With `windows_subsystem = "windows"` stderr goes nowhere, so panics are
/// appended to a log next to the executable instead of vanishing.
fn install_panic_hook() {
    const LOG_CAP_BYTES: u64 = 1_000_000;
    std::panic::set_hook(Box::new(|info| {
        let line = format!("panic: {info}\n");
        let log_path = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.join("lg-ultragear-rgb-control.log")));
        if let Some(path) = log_path {
            let oversized = std::fs::metadata(&path)
                .map(|m| m.len() > LOG_CAP_BYTES)
                .unwrap_or(false);
            if oversized {
                let _ = std::fs::write(&path, b"");
            }
            if let Ok(mut file) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
            {
                let _ = file.write_all(line.as_bytes());
                return;
            }
        }
        eprintln!("{line}");
    }));
}

/// Single instance guard: a named per-session mutex. A second launch exits
/// silently. The handle intentionally lives until process exit.
fn acquire_single_instance_lock() -> bool {
    use windows::core::w;
    use windows::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS};
    use windows::Win32::System::Threading::CreateMutexW;

    unsafe {
        match CreateMutexW(None, false, w!("Local\\LGUltraGearTray")) {
            Ok(_handle) => {
                if GetLastError() == ERROR_ALREADY_EXISTS {
                    eprintln!("Another instance is already running; exiting.");
                    return false;
                }
                true
            }
            Err(_) => true, // don't lock the user out if the mutex can't be made
        }
    }
}
