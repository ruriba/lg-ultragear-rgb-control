//! The dark color picker that replaces the light ChooseColor dialog: an HSV
//! square (pure tint whitening leftward, darkening downward), a hue slider
//! and a hex field, in the same fluent dark style as the panel. It lives on
//! the panel thread's event loop and is destroyed on close like the panel
//! itself. All color math is Rust-side; the UI reports raw interactions and
//! renders computed colors.

use crate::events::Events;
use crate::i18n::t;
use std::cell::RefCell;

use slint::ComponentHandle;

/// What the picker edits and which color it starts from. Target 0 is the
/// Audio Sync color; 1..=4 is a static slot.
pub enum PickerRequest {
    Slot(u8, [u8; 3]),
    Audio(Option<[u8; 3]>),
}

thread_local! {
    static PICKER_WIN: RefCell<Option<ColorPicker>> = const { RefCell::new(None) };
}

slint::slint! {
import { Button, LineEdit, Slider, Palette } from "std-widgets.slint";

export component ColorPicker inherits Window {
    title: root.title-text;
    in property <string> title-text;
    in property <string> ok-text;
    in property <string> cancel-text;
    in property <int> target;          // 1..=4 static slot, 0 = audio
    in property <color> initial;
    in-out property <float> hue;       // 0..360
    in-out property <float> sat;       // 0..1
    in-out property <float> val;       // 0..1
    in-out property <color> hue-color; // pure tint backing the square
    in-out property <color> shown;
    in-out property <string> hex-text;
    callback moved(float, float);      // sv square: x = saturation, y = 1-value
    callback hue-changed(float);
    callback hex-accepted();
    callback accept();
    callback cancel();
    preferred-width: 300px;
    min-width: 300px;
    max-width: 300px;
    preferred-height: 345px;
    min-height: 345px;
    max-height: 345px;
    background: Palette.background;
    // Same artwork as the exe resource, so the picker matches the panel.
    icon: @image-url("../assets/app.png");
    init => { Palette.color-scheme = ColorScheme.dark; }

    VerticalLayout {
        padding: 12px;
        spacing: 10px;

        // Saturation (x) over value (y): the pure tint whitens leftward and
        // darkens downward — the classic square without any shader.
        Rectangle {
            height: 150px;
            clip: true;
            border-width: 1px;
            border-color: Palette.border;
            Rectangle { background: root.hue-color; }
            Rectangle {
                background: @linear-gradient(90deg, #ffffffff 0%, #ffffff00 100%);
            }
            Rectangle {
                background: @linear-gradient(180deg, #00000000 0%, #000000ff 100%);
            }
            ta := TouchArea {
                moved => { root.moved(self.mouse-x / self.width * 1.0, self.mouse-y / self.height * 1.0); }
                clicked => { root.moved(self.mouse-x / self.width * 1.0, self.mouse-y / self.height * 1.0); }
            }
            Rectangle {
                width: 12px;
                height: 12px;
                border-radius: self.width / 2;
                border-width: 2px;
                border-color: white;
                x: root.sat * (parent.width - self.width);
                y: (1 - root.val) * (parent.height - self.height);
            }
        }

        // Hue strip (the affordance) + slider (the interaction).
        Rectangle {
            height: 10px;
            border-radius: 5px;
            background: @linear-gradient(90deg,
                #ff0000 0%, #ffff00 17%, #00ff00 33%, #00ffff 50%,
                #0000ff 67%, #ff00ff 83%, #ff0000 100%);
        }
        Slider {
            minimum: 0;
            maximum: 360;
            value <=> root.hue;
            changed(v) => { root.hue-changed(v); }
        }

        HorizontalLayout {
            spacing: 10px;
            Text {
                text: "Hex";
                width: 30px;
                vertical-alignment: center;
                color: Palette.foreground;
            }
            LineEdit {
                text <=> root.hex-text;
                accepted => { root.hex-accepted(); }
            }
        }

        HorizontalLayout {
            spacing: 10px;
            Rectangle {
                width: 60px;
                height: 24px;
                border-width: 1px;
                border-color: Palette.border;
                background: root.initial;
            }
            Rectangle {
                height: 24px;
                border-width: 1px;
                border-color: Palette.border;
                background: root.shown;
            }
        }

        HorizontalLayout {
            spacing: 10px;
            Button { text: root.ok-text; clicked => { root.accept(); } }
            Button { text: root.cancel-text; clicked => { root.cancel(); } }
        }
    }
}
}

/// Shows (or retargets) the picker window. Call from the panel thread; from
/// anywhere else, route through [`crate::panel::open_picker`].
pub fn show_picker(events: &Events, request: PickerRequest) {
    let (target, initial) = match request {
        PickerRequest::Slot(slot, rgb) => (i32::from(slot), rgb),
        PickerRequest::Audio(rgb) => (0, rgb.unwrap_or([255, 255, 255])),
    };

    // Already open: retarget it to the new row and reset to its color.
    PICKER_WIN.with(|w| {
        if let Some(p) = w.borrow().as_ref() {
            p.set_target(target);
            set_initial(p, initial);
        }
    });
    if PICKER_WIN.with(|w| w.borrow().is_some()) {
        center_picker();
        return;
    }

    let picker = match ColorPicker::new() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("panel: picker creation failed: {e}");
            return;
        }
    };
    picker.set_target(target);
    set_initial(&picker, initial);
    picker.set_title_text(slint::SharedString::from(t().pick_color));
    picker.set_ok_text(slint::SharedString::from(t().accept));
    picker.set_cancel_text(slint::SharedString::from(t().cancel));
    let ev = *events;
    picker.on_accept(move || {
        let _ = ev;
    });
    picker.on_cancel(close_picker);

    let weak = picker.as_weak();
    picker.on_moved(move |x, y| {
        let Some(p) = weak.upgrade() else { return };
        p.set_sat(x.clamp(0.0, 1.0));
        p.set_val(1.0 - y.clamp(0.0, 1.0));
        apply_current(&p);
    });
    let weak = picker.as_weak();
    picker.on_hue_changed(move |h| {
        let Some(p) = weak.upgrade() else { return };
        p.set_hue(h);
        apply_current(&p);
    });
    let weak = picker.as_weak();
    picker.on_hex_accepted(move || {
        let Some(p) = weak.upgrade() else { return };
        let text = p.get_hex_text();
        let clean: String = text.chars().filter(|c| c.is_ascii_hexdigit()).collect();
        if let Ok(v) = u32::from_str_radix(&clean, 16) {
            if clean.len() == 6 {
                let rgb = [(v >> 16) as u8, (v >> 8) as u8, v as u8];
                let (h, s, v) = rgb_to_hsv(rgb);
                p.set_hue(h);
                p.set_sat(s);
                p.set_val(v);
                apply_current(&p);
            }
        }
    });

    // Accept: report the result to the pump thread through the same event
    // channel every control uses, then close. Cancel just closes.
    let weak = picker.as_weak();
    picker.on_accept(move || {
        let Some(p) = weak.upgrade() else { return };
        let target = p.get_target();
        // Read back via the hex text (the shown color's canonical form).
        let text = p.get_hex_text();
        let clean: String = text.chars().filter(|ch| ch.is_ascii_hexdigit()).collect();
        let v = u32::from_str_radix(&clean, 16).unwrap_or(0xFFFFFF);
        let rgb = [(v >> 16) as u8, (v >> 8) as u8, v as u8];
        let id = if target == 0 {
            format!("__picker_audio:{},{},{}", rgb[0], rgb[1], rgb[2])
        } else {
            format!("__picker_slot_{target}:{},{},{}", rgb[0], rgb[1], rgb[2])
        };
        ev.send(id);
        close_picker();
    });

    picker.show().ok();
    if let Some(h) = crate::panel::native_hwnd_of(picker.window()) {
        crate::panel::apply_resource_icons(h);
    }
    PICKER_WIN.with(|w| *w.borrow_mut() = Some(picker));
    center_picker();
}

fn set_initial(picker: &ColorPicker, rgb: [u8; 3]) {
    picker.set_initial(slint::Color::from_rgb_u8(rgb[0], rgb[1], rgb[2]));
    let (h, s, v) = rgb_to_hsv(rgb);
    picker.set_hue(h);
    picker.set_sat(s);
    picker.set_val(v);
    picker.set_shown(slint::Color::from_rgb_u8(rgb[0], rgb[1], rgb[2]));
    picker.set_hex_text(slint::SharedString::from(hex_text(rgb)));
    picker.set_hue_color(hue_color(h));
}

/// Recomputes the shown color + hex from the current hue/sat/val.
fn apply_current(picker: &ColorPicker) {
    let (h, s, v) = (picker.get_hue(), picker.get_sat(), picker.get_val());
    let rgb = hsv_to_rgb(h, s, v);
    picker.set_hue_color(hue_color(h));
    picker.set_shown(slint::Color::from_rgb_u8(rgb[0], rgb[1], rgb[2]));
    picker.set_hex_text(slint::SharedString::from(hex_text(rgb)));
}

fn hex_text([r, g, b]: [u8; 3]) -> String {
    format!("#{r:02X}{g:02X}{b:02X}")
}

fn hue_color(h: f32) -> slint::Color {
    let [r, g, b] = hsv_to_rgb(h, 1.0, 1.0);
    slint::Color::from_rgb_u8(r, g, b)
}

/// HSV to RGB (h in degrees, s/v in 0..1).
fn hsv_to_rgb(h: f32, s: f32, v: f32) -> [u8; 3] {
    let c = v * s;
    let x = c * (1.0 - ((h / 60.0) % 2.0 - 1.0).abs());
    let m = v - c;
    let (r, g, b) = match h as u32 / 60 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    [
        ((r + m) * 255.0).round() as u8,
        ((g + m) * 255.0).round() as u8,
        ((b + m) * 255.0).round() as u8,
    ]
}

fn rgb_to_hsv([r, g, b]: [u8; 3]) -> (f32, f32, f32) {
    let (r, g, b) = (
        f32::from(r) / 255.0,
        f32::from(g) / 255.0,
        f32::from(b) / 255.0,
    );
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let d = max - min;
    let h = if d == 0.0 {
        0.0
    } else if max == r {
        60.0 * (((g - b) / d) % 6.0)
    } else if max == g {
        60.0 * ((b - r) / d + 2.0)
    } else {
        60.0 * ((r - g) / d + 4.0)
    };
    (
        h.rem_euclid(360.0),
        if max == 0.0 { 0.0 } else { d / max },
        max,
    )
}

/// Picker teardown: hides at once, then drops the window on the next loop
/// pass (destroying it inside its own button-callback dispatch is risky)
/// and hands the pages back to the OS.
pub fn close_picker() {
    PICKER_WIN.with(|w| {
        if let Some(p) = w.borrow().as_ref() {
            p.hide().ok();
        }
    });
    slint::Timer::single_shot(std::time::Duration::ZERO, || {
        PICKER_WIN.with(|w| drop(w.borrow_mut().take()));
        crate::panel::trim_working_set();
    });
}

/// Centers the picker on the work area of the monitor under the cursor.
fn center_picker() {
    PICKER_WIN.with(|w| {
        if let Some(p) = w.borrow().as_ref() {
            let window = p.window();
            let scale = f64::from(window.scale_factor());
            let size = window.size();
            let w = (300.0 * scale) as i32;
            let h = (345.0 * scale) as i32;
            let _ = size;
            if let Some((x, y)) = crate::panel::cursor_monitor_center(w, h) {
                window.set_position(slint::PhysicalPosition::new(x, y));
            }
        }
    });
}
