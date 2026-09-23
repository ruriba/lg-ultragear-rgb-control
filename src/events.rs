//! The app's single hidden top-level window and its message pump: every
//! cross-thread notification (menu clicks, USB connection changes, HID
//! device arrivals/removals, display topology changes, engine failures)
//! funnels through here as a queued [`String`] event.
//!
//! The window must be a real top-level window: WM_DISPLAYCHANGE is a
//! broadcast and broadcasts never reach message-only (HWND_MESSAGE) windows.

use std::sync::Mutex;

use windows::core::{s, w, PCSTR};
use windows::Win32::Devices::HumanInterfaceDevice::GUID_DEVINTERFACE_HID;
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress, LoadLibraryA};
use windows::Win32::UI::HiDpi::{
    SetProcessDpiAwareness, SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE,
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, PROCESS_PER_MONITOR_DPI_AWARE,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW, PostMessageW, RegisterClassW,
    RegisterDeviceNotificationW, TranslateMessage, DBT_DEVICEARRIVAL, DBT_DEVICEREMOVECOMPLETE,
    DBT_DEVTYP_DEVICEINTERFACE, DEV_BROADCAST_DEVICEINTERFACE_W, DEV_BROADCAST_HDR, MSG,
    PBT_APMRESUMEAUTOMATIC, PBT_APMRESUMESUSPEND, WINDOW_EX_STYLE, WM_APP, WM_DEVICECHANGE,
    WM_DISPLAYCHANGE, WM_POWERBROADCAST, WNDCLASSW, WS_OVERLAPPEDWINDOW,
};

/// UserEvent fired whenever the display topology changes.
pub const CHANGED_EVENT: &str = "__display_changed";

/// UserEvent fired when the system resumes from sleep.
pub const RESUME_EVENT: &str = "__resumed";

/// UserEvent fired when an HID device interface arrives or is removed
/// (WM_DEVICECHANGE): wakes the USB worker for an immediate presence check
/// instead of its next poll.
pub const DEVICE_EVENT: &str = "__device_event";

/// Events received but not yet dispatched. The wndproc only ever enqueues;
/// dispatch happens in the pump loop, where `&mut Ui` is held — modal dialogs
/// (the color picker) pump messages re-entrantly, so letting the wndproc call
/// the handler would alias the UI state.
static PENDING: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// Cloneable sender of user events. `PostMessageW` is thread-safe and an
/// HWND is just an integer handle, so this is freely shareable across
/// threads even though windows-rs marks HWND !Send.
#[derive(Clone, Copy)]
pub struct Events {
    hwnd: HWND,
}

unsafe impl Send for Events {}
unsafe impl Sync for Events {}

impl Events {
    /// Queues an event for the pump thread. Never blocks; on failure (window
    /// already gone, i.e. shutdown) the event is dropped.
    pub fn send(&self, event: String) {
        let boxed = Box::into_raw(Box::new(event));
        // SAFETY: the message window exists until process exit and the box
        // is reclaimed exactly once, by the WM_APP arm of the wndproc.
        if unsafe { PostMessageW(Some(self.hwnd), WM_APP, WPARAM(0), LPARAM(boxed as isize)) }
            .is_err()
        {
            drop(unsafe { Box::from_raw(boxed) });
        }
    }
}

/// Events queued since the last call, in arrival order.
pub fn drain() -> Vec<String> {
    PENDING
        .lock()
        .map(|mut p| std::mem::take(&mut *p))
        .unwrap_or_default()
}

/// Creates the hidden message window and returns its sender. Call once, on
/// the thread that will pump the messages (main).
pub fn init() -> Option<Events> {
    // Without the process being per-monitor DPI aware, Windows bitmap-scales
    // the menu/dialog UI on scaled displays (blurry menus, wrong hit rects).
    // Without the uxtheme AllowDark opt-in, muda cannot render the menu in
    // the system's dark theme. Both were set up by tao's event loop before;
    // init explicitly replicates them.
    unsafe { become_dpi_aware() };
    unsafe { allow_dark_mode() };
    let hwnd = unsafe { create_message_window()? };
    unsafe { register_hid_notifications(hwnd) };
    Some(Events { hwnd })
}

/// Subscribes the window to HID device-interface arrivals/removals, so a
/// monitor re-plug (or unplug) reaches the USB worker immediately instead of
/// on its next poll. Best-effort: without it the worker's periodic cadences
/// remain. The registration handle intentionally lives until process exit,
/// like the window and the single-instance mutex.
unsafe fn register_hid_notifications(hwnd: HWND) {
    let filter = DEV_BROADCAST_DEVICEINTERFACE_W {
        dbcc_size: std::mem::size_of::<DEV_BROADCAST_DEVICEINTERFACE_W>() as u32,
        dbcc_devicetype: DBT_DEVTYP_DEVICEINTERFACE.0,
        dbcc_reserved: 0,
        dbcc_classguid: GUID_DEVINTERFACE_HID,
        dbcc_name: [0; 1],
    };
    if RegisterDeviceNotificationW(
        hwnd.into(),
        &filter as *const _ as *const core::ffi::c_void,
        windows::Win32::UI::WindowsAndMessaging::DEVICE_NOTIFY_WINDOW_HANDLE,
    )
    .is_err()
    {
        eprintln!("Device notifications unavailable; falling back to USB polling only");
    }
}

unsafe fn create_message_window() -> Option<HWND> {
    let hinstance = GetModuleHandleW(None).unwrap_or_default();
    let class_name = w!("LGUltraGearTrayEvents");
    let wc = WNDCLASSW {
        lpfnWndProc: Some(wndproc),
        hInstance: HINSTANCE(hinstance.0),
        lpszClassName: class_name,
        ..Default::default()
    };
    RegisterClassW(&wc);
    // Hidden (no WS_VISIBLE) top-level window: never shown, never in the
    // taskbar; it exists only to receive messages.
    CreateWindowExW(
        WINDOW_EX_STYLE::default(),
        class_name,
        w!("LG UltraGear RGB Control"),
        WS_OVERLAPPEDWINDOW,
        0,
        0,
        0,
        0,
        None,
        None,
        Some(HINSTANCE(hinstance.0)),
        None,
    )
    .ok()
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_APP => {
            // SAFETY: the box was leaked by Events::send for exactly this
            // message and is never touched anywhere else.
            let event = *Box::from_raw(lparam.0 as *mut String);
            if let Ok(mut pending) = PENDING.lock() {
                pending.push(event);
            }
            LRESULT(0)
        }
        WM_DISPLAYCHANGE => {
            if let Ok(mut pending) = PENDING.lock() {
                pending.push(CHANGED_EVENT.to_string());
            }
            LRESULT(0)
        }
        WM_DEVICECHANGE => {
            // HID interface arrivals/removals only: other device-event kinds
            // (volume, port) are noise. A plug burst delivers one event per
            // interface; the handler's presence check is cheap and rare.
            if wparam.0 == DBT_DEVICEARRIVAL as usize
                || wparam.0 == DBT_DEVICEREMOVECOMPLETE as usize
            {
                // SAFETY: these DBT event kinds carry a DEV_BROADCAST_HDR
                // pointer in lParam per the WM_DEVICECHANGE contract.
                if let Some(hdr) = (lparam.0 as *const DEV_BROADCAST_HDR).as_ref() {
                    if hdr.dbch_devicetype == DBT_DEVTYP_DEVICEINTERFACE {
                        if let Ok(mut pending) = PENDING.lock() {
                            pending.push(DEVICE_EVENT.to_string());
                        }
                    }
                }
            }
            LRESULT(1) // TRUE: handled
        }
        WM_POWERBROADCAST => {
            // Resume from sleep: the monitor re-trains its link and the
            // lighting MCU can reset with no USB re-enumeration and no
            // WM_DISPLAYCHANGE. Broadcast notifications expect TRUE when
            // handled.
            if wparam.0 == PBT_APMRESUMEAUTOMATIC as usize
                || wparam.0 == PBT_APMRESUMESUSPEND as usize
            {
                if let Ok(mut pending) = PENDING.lock() {
                    pending.push(RESUME_EVENT.to_string());
                }
            }
            LRESULT(1)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

/// Per-monitor-v2 DPI awareness, the documented fallback chain aside. Same
/// setup the tao event loop performed on creation.
unsafe fn become_dpi_aware() {
    if SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2).is_err() {
        // V2 only works from Windows 10 1703; V1 from 1607.
        if SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE).is_err() {
            let _ = SetProcessDpiAwareness(PROCESS_PER_MONITOR_DPI_AWARE);
        }
    }
}

/// Opts the process into the system's dark theme via uxtheme's
/// SetPreferredAppMode (ordinal 135) so muda can render the tray menu dark.
/// Undocumented ordinals: absent on old systems, where GetProcAddress simply
/// returns None and the (light) default stays.
unsafe fn allow_dark_mode() {
    #[repr(i32)]
    #[allow(dead_code)]
    enum PreferredAppMode {
        Default = 0,
        AllowDark = 1,
    }
    type SetPreferredAppMode = unsafe extern "system" fn(PreferredAppMode) -> PreferredAppMode;
    type RefreshImmersiveColorPolicyState = unsafe extern "system" fn();

    let Ok(uxtheme) = LoadLibraryA(s!("uxtheme.dll")) else {
        return;
    };
    // Ordinals are passed as MAKEINTRESOURCE: the ID packed into the pointer.
    let ordinal = |n: usize| PCSTR::from_raw(n as *const u8);
    let set_mode: Option<SetPreferredAppMode> =
        std::mem::transmute(GetProcAddress(uxtheme, ordinal(135)));
    let refresh: Option<RefreshImmersiveColorPolicyState> =
        std::mem::transmute(GetProcAddress(uxtheme, ordinal(104)));
    if let Some(set_mode) = set_mode {
        set_mode(PreferredAppMode::AllowDark);
    }
    if let Some(refresh) = refresh {
        refresh();
    }
}

/// Blocks the calling thread pumping the message loop, dispatching queued
/// events through `handle` until it sets `quit` or the loop ends (WM_QUIT /
/// error).
pub fn pump(ui: &mut crate::menu::Ui, handle: fn(String, &mut crate::menu::Ui, &mut bool)) {
    let mut msg = MSG::default();
    let mut quit = false;
    while !quit {
        // GetMessage: >0 = dispatched, 0 = WM_QUIT, -1 = error (the classic
        // "-1 is truthy" trap — treat it as exit too).
        if unsafe { GetMessageW(&mut msg, None, 0, 0) }.0 <= 0 {
            break;
        }
        unsafe {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        for id in drain() {
            handle(id, ui, &mut quit);
            if quit {
                break;
            }
        }
    }
}
