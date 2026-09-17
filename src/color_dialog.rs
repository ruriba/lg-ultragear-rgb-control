//! Native Windows color picker (the ChooseColor common dialog): color wheel,
//! RGB sliders AND a hex field, everything the tray menu cannot offer. Menus
//! have no text input, so the "Custom" entries open this dialog instead.
//!
//! The dialog is centered on the monitor under the cursor via a WM_INITDIALOG
//! hook — ownerless common dialogs otherwise spawn at the top-left corner.

use std::sync::Mutex;
use windows::core::PCWSTR;
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MonitorFromPoint, MONITORINFO, MONITOR_DEFAULTTONEAREST,
};
use windows::Win32::UI::Controls::Dialogs::{
    ChooseColorW, CC_ENABLEHOOK, CC_FULLOPEN, CC_RGBINIT, CHOOSECOLORW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetCursorPos, GetWindowRect, SetWindowPos, SWP_NOSIZE, SWP_NOZORDER, WM_INITDIALOG,
};

/// Custom colors the dialog lets the user keep and reuse; lives as long as
/// the process. Modal on the UI thread, so the mutex is never contended.
static CUST_COLORS: Mutex<[COLORREF; 16]> = Mutex::new([COLORREF(0); 16]);

/// WM_INITDIALOG hook: recenters the dialog on the work area of the monitor
/// the cursor is on (the one the user clicked the tray from). Returns 0 so
/// the dialog's default processing continues.
unsafe extern "system" fn center_hook(hwnd: HWND, msg: u32, _w: WPARAM, _l: LPARAM) -> usize {
    if msg == WM_INITDIALOG {
        let mut rect = RECT::default();
        let _ = GetWindowRect(hwnd, &mut rect);
        let (w, h) = (rect.right - rect.left, rect.bottom - rect.top);
        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        let mut info = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if GetMonitorInfoW(MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST), &mut info).as_bool() {
            let x = info.rcWork.left + (info.rcWork.right - info.rcWork.left - w) / 2;
            let y = info.rcWork.top + (info.rcWork.bottom - info.rcWork.top - h) / 2;
            let _ = SetWindowPos(hwnd, None, x, y, 0, 0, SWP_NOSIZE | SWP_NOZORDER);
        }
    }
    0
}

/// Opens the color picker, preselecting `initial` (if any), and returns the
/// picked RGB triple — or None if the user cancelled.
pub fn pick_custom_color(initial: Option<[u8; 3]>) -> Option<[u8; 3]> {
    let rgb = initial.unwrap_or([255, 255, 255]);
    let mut cust = CUST_COLORS.lock().unwrap();
    let mut cc = CHOOSECOLORW {
        lStructSize: std::mem::size_of::<CHOOSECOLORW>() as u32,
        hwndOwner: HWND::default(),
        rgbResult: COLORREF(
            u32::from(rgb[0]) | (u32::from(rgb[1]) << 8) | (u32::from(rgb[2]) << 16),
        ),
        lpCustColors: cust.as_mut_ptr(),
        // FULLOPEN: the custom-colors pane (with the hex field) opens ready.
        Flags: CC_FULLOPEN | CC_RGBINIT | CC_ENABLEHOOK,
        lCustData: LPARAM::default(),
        lpfnHook: Some(center_hook),
        lpTemplateName: PCWSTR::null(),
        hInstance: HWND::default(),
    };
    unsafe {
        if !ChooseColorW(&mut cc).as_bool() {
            return None; // cancelled
        }
        let c = cc.rgbResult.0;
        Some([
            (c & 0xFF) as u8,
            ((c >> 8) & 0xFF) as u8,
            ((c >> 16) & 0xFF) as u8,
        ])
    }
}
