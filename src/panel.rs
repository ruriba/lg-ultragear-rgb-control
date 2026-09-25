//! The control panel: a Slint window (fluent style, forced dark scheme)
//! mirroring the tray menu with live controls. It is a *view* of the same
//! [`crate::menu::Ui`] state, not a second owner:
//!
//! - Input: every widget callback is translated into the same event id the
//!   menu uses (`"mode_3"`, `"toggle_audio"`, …) and posted through
//!   [`Events::send`] — identical ids, so menu and panel can never disagree.
//! - Output: [`sync`] snapshots `Ui` after each event (right after
//!   `sync_menu`) and ships it to the Slint thread with
//!   [`slint::invoke_from_event_loop`], which wakes the event loop exactly
//!   once per update — no polling timer, no idle wakeups.
//!
//! Layout: one lighting-mode selector (statics, LG animations, both syncs)
//! in a left column; the right column holds the transversal brightness plus
//! ONE contextual section, swapped to the active mode — a static mode shows
//! only its own color, Image Sync its tunings, Audio Sync its tunings, and
//! modes without settings show a placeholder. LED power lives in the header,
//! autostart in the footer. The window is fixed-size at its content minimum
//! (min == preferred): a control panel gains nothing from resizing and can
//! never be shrunk below its content. The unified selector keeps pure radio
//! semantics — picking the option that is already active is a no-op, so the
//! callbacks consult the latest snapshot before toggling.
//!
//! Lifetime: the panel thread is spawned once and lives for the process, but
//! the *window* exists only while it is on screen. Close destroys it and
//! quits the Slint event loop (freeing the softbuffer surface, the renderer
//! buffers and the component tree; the working set is trimmed right after),
//! and the thread parks on a channel until the next open builds a fresh
//! window and re-runs the loop — winit's `EventLoop` cannot be recreated, so
//! the thread (and the loop instance it keeps) is reused, exactly the
//! pattern `run_event_loop_until_quit`'s generation counter supports. This
//! is why the previous "hide the window and keep everything alive" approach
//! was replaced: a closed panel now costs no renderer memory at all.
//!
//! The repaint fix: the software renderer blits through softbuffer outside
//! `WM_PAINT` and skips the blit when its dirty region is empty, but
//! Windows recreates the window's redirection surface from scratch after a
//! minimize/restore — if no property changed, the restored surface stays
//! black. The window is therefore subclassed and every `WM_PAINT` (i.e.
//! every "the surface is invalid, repaint" request from the system) flips
//! the `force-repaint` property, whose near-invisible full-window overlay
//! guarantees a full-window dirty region and a full blit.

use crate::audio::{AudioColor, Blink, DynamicRange};
use crate::events::Events;
use crate::i18n::t;
use crate::menu::{static_mode_name, SyncActive, Ui, BOOST, FPS, GAIN, LANGUAGES, SMOOTHING};
pub use crate::picker::PickerRequest;
use crate::picker::{show_picker, ColorPicker};
use crate::sampling::SamplingMode;
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::Mutex;
use std::time::Duration;

slint::slint! {
import { ComboBox, GroupBox, Palette, Slider, Switch } from "std-widgets.slint";

// The description shown in the window's manual strip while a row label is
// hovered (interactive manual, see the HintLabel component).
export global Hint {
    in-out property <string> text;
}

// A row label that explains itself: hovering it shows the setting's
// manual description in the window's hint strip.
component HintLabel inherits Rectangle {
    in property <string> label;
    in property <string> hint;
    ta := TouchArea {
        mouse-cursor: help;
        changed has-hover => {
            if (ta.has-hover) {
                Hint.text = root.hint;
            } else if (Hint.text == root.hint) {
                Hint.text = "";
            }
        }
    }
    Text {
        text: root.label;
        vertical-alignment: center;
        color: Palette.foreground;
    }
}

// The ONE color entry, shared by the static modes and Audio Sync: a card
// whose left chip carries the state — the solid color, or a rainbow sweep
// while that is active — and whose label names it (the hex, or "Arcoíris").
// Clicking it opens the native color picker.
component ColorCard inherits Rectangle {
    in property <color> stripe-color;
    in property <bool> stripe-solid;
    in property <string> label;
    callback pick();
    height: 34px;
    border-radius: 6px;
    background: Palette.control-background;
    border-width: 1px;
    border-color: Palette.border;
    ta := TouchArea {
        mouse-cursor: pointer;
        clicked => { root.pick(); }
    }
    HorizontalLayout {
        padding: 4px;
        padding-left: 6px;
        spacing: 10px;
        if root.stripe-solid : Rectangle {
            width: 26px;
            border-radius: 4px;
            background: root.stripe-color;
            border-width: 1px;
            border-color: Palette.border;
        }
        if !root.stripe-solid : Rectangle {
            width: 26px;
            border-radius: 4px;
            border-width: 1px;
            border-color: Palette.border;
            background: @linear-gradient(90deg,
                #ff0000 0%, #ffff00 20%, #00ff00 40%,
                #00ffff 60%, #0000ff 80%, #ff00ff 100%);
        }
        Text {
            text: root.label;
            vertical-alignment: center;
            color: Palette.foreground;
        }
    }
}

// Ordered discrete presets as a segmented row of pills: every option is
// visible at once, one click switches, and the active one is filled with
// the style accent — a slider's continuous drag would suggest values that
// do not exist.
component Segments inherits Rectangle {
    in property <[string]> options;
    in property <int> active-index;
    callback select(int);
    height: 34px;
    HorizontalLayout {
        spacing: 4px;
        for opt[i] in root.options : Rectangle {
            background: i == root.active-index ? Palette.accent-background : Palette.control-background;
            border-width: 1px;
            border-color: i == root.active-index ? Palette.accent-background : Palette.border;
            border-radius: 6px;
            min-width: 64px;
            ta := TouchArea {
                mouse-cursor: pointer;
                clicked => { root.select(i); }
            }
            Text {
                text: opt;
                color: i == root.active-index ? Palette.accent-foreground : Palette.foreground;
                font-weight: i == root.active-index ? 600 : 400;
                horizontal-alignment: center;
                vertical-alignment: center;
            }
        }
    }
}

// Label + slider + live value readout. The value width is overridable:
// preset sliders show a word ("Normal") instead of a short number.
component SliderRow inherits Rectangle {
    in property <string> label;
    in property <string> hint;
    in-out property <float> value;
    in property <float> minimum;
    in property <float> maximum;
    in property <float> step;
    in property <string> value-text;
    in property <length> value-width: 44px;
    callback value-changed(float);
    HorizontalLayout {
        spacing: 10px;
        HintLabel {
            label: root.label;
            hint: root.hint;
            width: 104px;
        }
        s := Slider {
            minimum: root.minimum;
            maximum: root.maximum;
            step: root.step;
            value <=> root.value;
            changed(v) => { root.value-changed(v); }
        }
        Text {
            text: root.value-text;
            width: root.value-width;
            horizontal-alignment: right;
            vertical-alignment: center;
            color: Palette.foreground;
        }
    }
}

// One lighting-mode entry, drawn like a fluent radio (ring + accent dot
// when active). Hand-drawn instead of a RadioGroup so the software syncs
// can sit in their own section: RadioGroup only allows RadioButton
// children, and two groups could not keep the cross-group exclusivity.
component ModeOption inherits Rectangle {
    in property <string> label;
    in property <bool> active;
    callback selected();
    height: 30px;
    ta := TouchArea {
        mouse-cursor: pointer;
        clicked => { root.selected(); }
    }
    HorizontalLayout {
        padding-left: 6px;
        spacing: 8px;
        Rectangle {
            width: 18px;
            height: 18px;
            y: (parent.height - self.height) / 2;
            border-radius: self.width / 2;
            border-width: 1px;
            border-color: root.active ? Palette.accent-background : Palette.border;
            background: Palette.control-background;
            Rectangle {
                width: 10px;
                height: 10px;
                x: (parent.width - self.width) / 2;
                y: (parent.height - self.height) / 2;
                border-radius: self.width / 2;
                background: root.active ? Palette.accent-background : transparent;
            }
        }
        Text {
            text: root.label;
            vertical-alignment: center;
            color: Palette.foreground;
        }
    }
}

export component PanelWindow inherits Window {
    title: "LG UltraGear RGB Control";
    // Fixed size, period: min == max means the winit backend creates the
    // window non-resizable with no maximize button (sized to the widest
    // row — the labeled sampling radios — with no dead space; the height
    // fits the mode selector exactly, the tallest thing in either column:
    // 8 x 30px radio rows + the section divider + header/separator/status
    // rows and paddings).
    preferred-width: 670px;
    preferred-height: 372px;
    min-width: 670px;
    min-height: 372px;
    max-width: 670px;
    max-height: 372px;
    background: Palette.background;
    // Same artwork as the exe resource, so the window matches the tray.
    icon: @image-url("../assets/app.png");
    init => { Palette.color-scheme = ColorScheme.dark; }

    // State in, snapshotted from the menu thread after every event.
    in property <string> status-text;
    // Strip power, shown and driven by the header switch.
    in-out property <bool> leds-on;
    // Which contextual view the right column shows: 0 placeholder,
    // 1 static color, 2 image sync tunings, 3 audio sync tunings.
    in property <int> active-view;
    in property <string> view-hint;
    // The active static mode's own color slot (1..=4 when view == 1).
    in property <int> active-slot;
    in property <color> slot-color;
    in property <string> slot-hex-text;
    // One selector for everything the strip can do. Radio semantics: the
    // two-way bools are driven by the snapshot; user picks arrive through
    // the group's `selected` callback only.
    in-out property <bool> mode1-checked;
    in-out property <bool> mode2-checked;
    in-out property <bool> mode3-checked;
    in-out property <bool> mode4-checked;
    in-out property <bool> mode5-checked;
    in-out property <bool> mode6-checked;
    in-out property <bool> imgsync-checked;
    in-out property <bool> audsync-checked;
    in property <float> brightness;
    in property <string> brightness-text;
    // Tuning presets as segmented pills, straight from the menu's tables.
    in property <[string]> smooth-options;
    in property <int> smooth-index;
    in property <[string]> boost-options;
    in property <int> boost-index;
    in property <[string]> fps-options;
    in property <int> fps-index;
    in property <[string]> gain-options;
    in property <int> gain-index;
    in-out property <bool> rainbow;
    in property <color> solid-color;
    in property <bool> solid-set;
    in property <string> solid-text;
    // Discrete preset rows (Muestreo, Parpadeo, Rango dinámico) as
    // segmented pills: the option list plus the active index.
    in property <[string]> sample-options;
    in property <int> sample-index;
    in property <[string]> blink-options;
    in property <int> blink-index;
    in property <[string]> range-options;
    in property <int> range-index;
    // Interactive-manual descriptions, keyed by row.
    in property <string> hint-brightness;
    in property <string> hint-sampling;
    in property <string> hint-smoothing;
    in property <string> hint-boost;
    in property <string> hint-fps;
    in property <string> hint-static-color;
    in property <string> hint-audio-color;
    in property <string> hint-sensitivity;
    in property <string> hint-blink;
    in property <string> hint-range;
    in property <[string]> lang-options;
    in property <int> lang-index;
    in-out property <bool> autostart;
    // Repaint trigger: flipping this value dirties the full-window overlay
    // below, forcing the software renderer to blit the whole window even
    // when its own dirty tracking is empty (see the module docs). The two
    // overlay colors are ~1.5% alpha black: invisible either way.
    in-out property <bool> force-repaint;

    // Labels (i18n lives on the menu thread).
    in property <string> lbl-bright;
    in property <string> lbl-color;
    in property <string> lbl-sampling;
    in property <string> lbl-sens;
    in property <string> lbl-smooth;
    in property <string> lbl-boost;
    in property <string> lbl-blink;
    in property <string> lbl-range;
    in property <string> lbl-rainbow;
    in property <string> lbl-autostart;
    in property <string> lbl-language;
    in property <string> mode1-text;
    in property <string> mode2-text;
    in property <string> mode3-text;
    in property <string> mode4-text;
    in property <string> mode5-text;
    in property <string> mode6-text;
    // lighting-selected: 0..5 → static/animation modes, 6 → image sync,
    // 7 → audio sync (the Rust side no-ops an already-active pick).
    callback lighting-selected(int);
    // `on` is the switch state AFTER the toggle: true = turn the strip on,
    // false = power it off (a running sync stops and stays remembered).
    callback power-changed(bool);
    callback brightness-changed(float);
    callback smooth-select(int);
    callback boost-select(int);
    callback fps-select(int);
    callback gain-select(int);
    callback sampling-select(int);
    callback slot-pick(int);
    // `on` is the switch state AFTER the user's toggle: true = sweep on,
    // false = restore the remembered solid color.
    callback rainbow-changed(bool);
    callback solid-pick();
    callback blink-select(int);
    callback range-select(int);
    callback autostart-toggled();
    callback lang-select(string);

    // Plain Rectangle root so the repaint overlay can sit above the layout
    // without taking part in it (a Window cannot absolutely place children
    // next to a layout).
    Rectangle {
        VerticalLayout {
            spacing: 10px;
            padding: 14px;

            // Header: strip power on the left; autostart and the language
            // selector on the right, with empty space in between.
            HorizontalLayout {
                spacing: 10px;
                Switch {
                    // Language-neutral control label, per user preference.
                    text: "LEDs on/off";
                    checked <=> root.leds-on;
                    toggled => { root.power-changed(root.leds-on); }
                }
                Rectangle { }
                Switch {
                    text: root.lbl-autostart;
                    checked <=> root.autostart;
                    toggled => { root.autostart-toggled(); }
                }
                Rectangle {
                    width: 1px;
                    background: Palette.border;
                }
                Text {
                    text: root.lbl-language;
                    width: 50px;
                    vertical-alignment: center;
                    color: Palette.foreground;
                }
                ComboBox {
                    width: 140px;
                    model: root.lang-options;
                    current-index: root.lang-index;
                    selected(value) => { root.lang-select(value); }
                }
            }

            Rectangle {
                height: 1px;
                background: Palette.border;
            }

            HorizontalLayout {
                spacing: 14px;

                // Everything the strip can do, in one column. The monitor's
                // own modes first; the software syncs in their own section,
                // like the tray menu's Mode submenu.
                VerticalLayout {
                    alignment: start;
                    spacing: 0px;
                    width: 150px;
                    ModeOption {
                        label: root.mode1-text;
                        active: root.mode1-checked;
                        selected => { root.lighting-selected(0); }
                    }
                    ModeOption {
                        label: root.mode2-text;
                        active: root.mode2-checked;
                        selected => { root.lighting-selected(1); }
                    }
                    ModeOption {
                        label: root.mode3-text;
                        active: root.mode3-checked;
                        selected => { root.lighting-selected(2); }
                    }
                    ModeOption {
                        label: root.mode4-text;
                        active: root.mode4-checked;
                        selected => { root.lighting-selected(3); }
                    }
                    ModeOption {
                        label: root.mode5-text;
                        active: root.mode5-checked;
                        selected => { root.lighting-selected(4); }
                    }
                    ModeOption {
                        label: root.mode6-text;
                        active: root.mode6-checked;
                        selected => { root.lighting-selected(5); }
                    }
                    VerticalLayout {
                        padding-top: 5px;
                        padding-bottom: 5px;
                        Rectangle {
                            height: 1px;
                            background: Palette.border;
                        }
                    }
                    ModeOption {
                        label: "Image Sync";
                        active: root.imgsync-checked;
                        selected => { root.lighting-selected(6); }
                    }
                    ModeOption {
                        label: "Audio Sync";
                        active: root.audsync-checked;
                        selected => { root.lighting-selected(7); }
                    }
                }

                // Vertical hairline between the mode selector and the
                // configuration column.
                Rectangle {
                    width: 1px;
                    background: Palette.border;
                }

                VerticalLayout {
                    alignment: start;
                    spacing: 10px;

                    // Transversal: meaningful in every mode (software dimming
                    // while a sync runs).
                    SliderRow {
                        label: root.lbl-bright;
                        hint: root.hint-brightness;
                        value: root.brightness;
                        minimum: 1;
                        maximum: 12;
                        step: 1;
                        value-text: root.brightness-text;
                        value-changed(v) => { root.brightness-changed(v); }
                    }

                    // Contextual separator: everything below belongs to the
                    // active mode only (its name is already highlighted in the
                    // selector — no title needed). Rows keep their natural
                    // height, packed top-down.
                    Rectangle {
                        height: 1px;
                        background: Palette.border;
                    }

                    VerticalLayout {
                        alignment: start;
                        spacing: 8px;

                        // 0: no settings for this mode (or none chosen yet).
                        if root.active-view == 0 : Text {
                            text: root.view-hint;
                            min-height: 28px;
                            horizontal-alignment: center;
                            vertical-alignment: center;
                            color: Palette.foreground;
                        }

                        // 1: a static mode shows exactly its own color,
                        // with the same card the audio color uses.
                        if root.active-view == 1 : HorizontalLayout {
                            spacing: 10px;
                            HintLabel {
                                label: root.lbl-color;
                                hint: root.hint-static-color;
                                width: 104px;
                            }
                            ColorCard {
                                width: 170px;
                                stripe-color: root.slot-color;
                                stripe-solid: true;
                                label: root.slot-hex-text;
                                pick => { root.slot-pick(root.active-slot); }
                            }
                            Rectangle { }
                        }

                        // 2: Image Sync tunings.
                        if root.active-view == 2 : VerticalLayout {
                            spacing: 8px;
                            HorizontalLayout {
                                spacing: 10px;
                                HintLabel {
                                    label: root.lbl-sampling;
                                    hint: root.hint-sampling;
                                    width: 104px;
                                }
                                Segments {
                                    options: root.sample-options;
                                    active-index: root.sample-index;
                                    select(i) => { root.sampling-select(i); }
                                }
                            }
                            HorizontalLayout {
                                spacing: 10px;
                                HintLabel {
                                    label: root.lbl-smooth;
                                    hint: root.hint-smoothing;
                                    width: 104px;
                                }
                                Segments {
                                    options: root.smooth-options;
                                    active-index: root.smooth-index;
                                    select(i) => { root.smooth-select(i); }
                                }
                            }
                            HorizontalLayout {
                                spacing: 10px;
                                HintLabel {
                                    label: root.lbl-boost;
                                    hint: root.hint-boost;
                                    width: 104px;
                                }
                                Segments {
                                    options: root.boost-options;
                                    active-index: root.boost-index;
                                    select(i) => { root.boost-select(i); }
                                }
                            }
                            HorizontalLayout {
                                spacing: 10px;
                                HintLabel {
                                    label: "FPS";
                                    hint: root.hint-fps;
                                    width: 104px;
                                }
                                Segments {
                                    options: root.fps-options;
                                    active-index: root.fps-index;
                                    select(i) => { root.fps-select(i); }
                                }
                            }
                        }

                        // 3: Audio Sync tunings, color first (the most
                        // visual setting).
                        if root.active-view == 3 : VerticalLayout {
                            spacing: 8px;
                            HorizontalLayout {
                                spacing: 10px;
                                HintLabel {
                                    label: root.lbl-color;
                                    hint: root.hint-audio-color;
                                    width: 104px;
                                }
                                ColorCard {
                                    width: 170px;
                                    stripe-color: root.solid-color;
                                    stripe-solid: root.solid-set;
                                    label: root.solid-text;
                                    pick => { root.solid-pick(); }
                                }
                                Switch {
                                    text: root.lbl-rainbow;
                                    checked <=> root.rainbow;
                                    toggled => { root.rainbow-changed(root.rainbow); }
                                }
                            }
                            HorizontalLayout {
                                spacing: 10px;
                                HintLabel {
                                    label: root.lbl-sens;
                                    hint: root.hint-sensitivity;
                                    width: 104px;
                                }
                                Segments {
                                    options: root.gain-options;
                                    active-index: root.gain-index;
                                    select(i) => { root.gain-select(i); }
                                }
                            }
                            HorizontalLayout {
                                spacing: 10px;
                                HintLabel {
                                    label: root.lbl-blink;
                                    hint: root.hint-blink;
                                    width: 104px;
                                }
                                Segments {
                                    options: root.blink-options;
                                    active-index: root.blink-index;
                                    select(i) => { root.blink-select(i); }
                                }
                            }
                            HorizontalLayout {
                                spacing: 10px;
                                HintLabel {
                                    label: root.lbl-range;
                                    hint: root.hint-range;
                                    width: 104px;
                                }
                                Segments {
                                    options: root.range-options;
                                    active-index: root.range-index;
                                    select(i) => { root.range-select(i); }
                                }
                            }
                        }
                    }
                }
            }

            // Status bar: connection/diagnostics normally; while a row's label
            // is hovered it becomes the interactive manual (opaque card).
            Rectangle {
                height: 30px;
                background: Hint.text != "" ? Palette.control-background : transparent;
                border-width: Hint.text != "" ? 1px : 0px;
                border-color: Palette.border;
                border-radius: Hint.text != "" ? 6px : 0px;
                Text {
                    text: Hint.text != "" ? Hint.text : root.status-text;
                    color: Palette.foreground;
                    vertical-alignment: center;
                    wrap: word-wrap;
                }
            }
        }

        // Full-window repaint overlay (see `force-repaint`): covers the
        // window, paints at an invisible ~1.5% black alpha, and only exists
        // so that flipping its color dirties the entire window.
        Rectangle {
            x: 0;
            y: 0;
            width: 100%;
            height: 100%;
            background: root.force-repaint ? #03030303 : #04040404;
        }
    }
}
}

/// One state snapshot, produced on the pump thread after every event and
/// applied to the window on the Slint thread. Labels ride along: i18n lives
/// on the pump thread and applies on next launch, like the menu's own texts.
#[derive(Clone)]
struct Snapshot {
    status: String,
    /// Which contextual view the right column shows: 0 placeholder,
    /// 1 static color, 2 image sync, 3 audio sync.
    view: i32,
    /// Whether the strip is lit (drives the header power switch).
    leds_on: bool,
    /// Placeholder text for view 0.
    view_hint: &'static str,
    /// The active static mode's slot, 1..=4 while view == 1.
    active_slot: u8,
    /// Fill color of the active static slot's card chip.
    slot_color: slint::Color,
    /// Card label for the static view: the hex, or a dash while unset.
    slot_hex: String,
    sync_image: bool,
    sync_audio: bool,
    brightness: f32,
    brightness_text: String,
    /// The static/animation mode radio that is checked, if any (a running
    /// sync checks its own radio instead).
    mode: Option<u8>,
    /// Selected UI language, as an index into LANGUAGES.
    lang_index: i32,
    /// Tuning preset indices, straight from the menu's tables.
    smooth_index: i32,
    boost_index: i32,
    fps_index: i32,
    gain_index: i32,
    rainbow: bool,
    solid: slint::Color,
    solid_set: bool,
    /// Card label for the audio view: the hex while a solid shows, or the
    /// rainbow name while the sweep runs.
    solid_text: String,
    /// Discrete preset rows: localized option lists plus the active index
    /// (Muestreo 0..=2, Parpadeo 0..=2, Rango dinámico 0..=3). The models
    /// themselves are built on the Slint thread in [`apply`] — ModelRc is
    /// not Send, but the labels ride along anyway.
    sample_index: i32,
    blink_index: i32,
    range_index: i32,
    autostart: bool,
    labels: Labels,
}

/// Every translated string the panel shows, resolved once per snapshot.
#[derive(Clone)]
struct Labels {
    bright: &'static str,
    color: &'static str,
    sampling: &'static str,
    sens: &'static str,
    smooth: &'static str,
    boost: &'static str,
    blink: &'static str,
    range: &'static str,
    rainbow: &'static str,
    connected: &'static str,
    disconnected: &'static str,
    autostart: &'static str,
    language: &'static str,
    modes: [String; 6],
    samples: [&'static str; 3],
    blinks: [&'static str; 3],
    ranges: [&'static str; 4],
    smooths: [&'static str; 4],
    boosts: [&'static str; 4],
    fpses: [&'static str; 4],
    gains: [&'static str; 3],
    hints: Hints,
    langs: [&'static str; 11],
    no_settings: &'static str,
    select_mode: &'static str,
}

impl Labels {
    fn resolve() -> Self {
        Self {
            bright: t().brightness,
            color: t().color,
            sampling: t().sampling,
            sens: t().sensitivity,
            smooth: t().smoothing,
            boost: t().boost,
            blink: t().blink,
            range: t().dynamic_range,
            rainbow: t().rainbow,
            connected: t().connected,
            disconnected: t().disconnected,
            autostart: t().autostart,
            language: t().language,
            modes: [
                static_mode_name(1),
                static_mode_name(2),
                static_mode_name(3),
                static_mode_name(4),
                static_mode_name(5),
                static_mode_name(6),
            ],
            samples: [t().border5, t().border15, t().full_screen],
            blinks: [t().blink_smooth, t().blink_normal, t().blink_fast],
            ranges: [
                t().range_compressed,
                t().range_normal,
                t().range_expanded,
                t().range_extreme,
            ],
            smooths: std::array::from_fn(|i| (SMOOTHING[i].0)(t())),
            boosts: std::array::from_fn(|i| (BOOST[i].0)(t())),
            fpses: std::array::from_fn(|i| (FPS[i].0)(t())),
            gains: std::array::from_fn(|i| (GAIN[i].0)(t())),
            hints: Hints::resolve(),
            langs: language_labels(),
            no_settings: t().no_settings,
            select_mode: t().select_mode,
        }
    }
}

/// The interactive-manual descriptions, one per labeled row.
#[derive(Clone, Copy)]
struct Hints {
    brightness: &'static str,
    sampling: &'static str,
    smoothing: &'static str,
    boost: &'static str,
    fps: &'static str,
    static_color: &'static str,
    audio_color: &'static str,
    sensitivity: &'static str,
    blink: &'static str,
    range: &'static str,
}

impl Hints {
    fn resolve() -> Self {
        Self {
            brightness: t().hint_brightness,
            sampling: t().hint_sampling,
            smoothing: t().hint_smoothing,
            boost: t().hint_boost,
            fps: t().hint_fps,
            static_color: t().hint_static_color,
            audio_color: t().hint_audio_color,
            sensitivity: t().hint_sensitivity,
            blink: t().hint_blink,
            range: t().hint_range,
        }
    }
}

/// Hex label for a color card chip ("#RRGGBB").
fn hex_text([r, g, b]: [u8; 3]) -> String {
    format!("#{r:02X}{g:02X}{b:02X}")
}

/// Index of a tuning preset by value; unknown (legacy) values show the
/// first option until the user picks one.
fn preset_index(value: f32, values: impl Iterator<Item = f32>) -> i32 {
    let mut values = values;
    values.position(|v| v == value).map_or(0, |i| i as i32)
}

/// The tray language list as display labels: the System entry follows the
/// active language; the rest are fixed endonyms.
fn language_labels() -> [&'static str; 11] {
    std::array::from_fn(|i| if i == 0 { t().system } else { LANGUAGES[i].0 })
}

/// A preset table as a Slint string model (one entry per segment).
fn string_model(items: &[&'static str]) -> slint::ModelRc<slint::SharedString> {
    slint::ModelRc::new(slint::VecModel::from(
        items
            .iter()
            .map(|s| slint::SharedString::from(*s))
            .collect::<Vec<_>>(),
    ))
}

fn snapshot_from(ui: &Ui) -> Snapshot {
    let labels = Labels::resolve();
    // The header shows connection and diagnostics only: the running mode
    // is already visible in the selector, so repeating it here (the tray
    // status line does) read as a mystery label.
    let connection = if ui.connected.load(Ordering::SeqCst) {
        labels.connected
    } else {
        labels.disconnected
    };
    // The right column follows the active lighting mode: a running sync
    // owns the view; otherwise the static mode does (1..=4 get their color,
    // the LG animations get the placeholder, and a never-set state gets the
    // "pick a mode" hint).
    // The active configuration names the view even when nothing runs: a
    // sync picked while the LEDs are off (pending) or the one remembered
    // for resume after a power-off keep their view and radio instead of
    // falling back to the "pick a mode" placeholder.
    let shown_sync = if ui.sync != SyncActive::None {
        ui.sync
    } else {
        ui.pending_boot_sync
            .or(ui.resume)
            .unwrap_or(SyncActive::None)
    };
    let (view, view_hint, active_slot): (i32, &'static str, u8) = match shown_sync {
        SyncActive::ImageSync => (2, "", 0),
        SyncActive::Audio => (3, "", 0),
        SyncActive::None => match ui.mode {
            Some(m @ 1..=4) => (1, "", m),
            Some(5..=6) => (0, labels.no_settings, 0),
            _ => (0, labels.select_mode, 0),
        },
    };
    let fill = if active_slot > 0 {
        ui.slot_colors[(active_slot - 1) as usize]
    } else {
        None
    };
    let (solid_color, solid_set, solid_text) = match ui.audio.color {
        AudioColor::Solid(rgb) => (
            slint::Color::from_rgb_u8(rgb[0], rgb[1], rgb[2]),
            true,
            hex_text(rgb),
        ),
        AudioColor::Rainbow => (
            slint::Color::from_rgb_u8(255, 255, 255),
            false,
            labels.rainbow.to_string(),
        ),
    };
    Snapshot {
        status: ui.note.clone().unwrap_or_else(|| connection.to_string()),
        view,
        leds_on: ui.leds_on,
        view_hint,
        active_slot,
        slot_color: fill.map_or(slint::Color::from_rgb_u8(0x40, 0x40, 0x40), |[r, g, b]| {
            slint::Color::from_rgb_u8(r, g, b)
        }),
        slot_hex: fill.map_or("—".to_string(), hex_text),
        lang_index: LANGUAGES
            .iter()
            .position(|(_, l)| *l == ui.language)
            .unwrap_or(0) as i32,
        sync_image: shown_sync == SyncActive::ImageSync,
        sync_audio: shown_sync == SyncActive::Audio,
        brightness: f32::from(ui.brightness.unwrap_or(12)),
        brightness_text: ui.brightness.unwrap_or(12).to_string(),
        mode: ui.mode.filter(|m| (1..=6).contains(m)),
        sample_index: match ui.params.sampling {
            SamplingMode::Border5 => 0,
            SamplingMode::Border15 => 1,
            SamplingMode::Full => 2,
        },
        smooth_index: preset_index(ui.params.smoothing, SMOOTHING.iter().map(|(_, v)| *v)),
        boost_index: preset_index(ui.params.boost, BOOST.iter().map(|(_, v)| *v)),
        fps_index: preset_index(ui.params.fps as f32, FPS.iter().map(|(_, v)| *v as f32)),
        gain_index: preset_index(ui.audio.gain, GAIN.iter().map(|(_, v)| *v)),
        rainbow: ui.audio.color == AudioColor::Rainbow,
        solid: solid_color,
        solid_set,
        solid_text,
        blink_index: match ui.audio.blink {
            Blink::Smooth => 0,
            Blink::Normal => 1,
            Blink::Fast => 2,
        },
        range_index: match ui.audio.range {
            DynamicRange::Compressed => 0,
            DynamicRange::Normal => 1,
            DynamicRange::Expanded => 2,
            DynamicRange::Extreme => 3,
        },
        autostart: ui.autostart,
        labels,
    }
}

fn apply(panel: &PanelWindow, s: &Snapshot) {
    use slint::SharedString;
    let txt = |s: &str| SharedString::from(s.to_string());
    panel.set_status_text(txt(&s.status));
    panel.set_active_view(s.view);
    panel.set_leds_on(s.leds_on);
    panel.set_view_hint(txt(s.view_hint));
    panel.set_active_slot(i32::from(s.active_slot));
    panel.set_slot_color(s.slot_color);
    panel.set_slot_hex_text(txt(&s.slot_hex));
    // One checked radio in the selector: the running sync's, or the static
    // mode's while idle.
    panel.set_imgsync_checked(s.sync_image);
    panel.set_audsync_checked(s.sync_audio);
    panel.set_mode1_checked(!s.sync_image && !s.sync_audio && s.mode == Some(1));
    panel.set_mode2_checked(!s.sync_image && !s.sync_audio && s.mode == Some(2));
    panel.set_mode3_checked(!s.sync_image && !s.sync_audio && s.mode == Some(3));
    panel.set_mode4_checked(!s.sync_image && !s.sync_audio && s.mode == Some(4));
    panel.set_mode5_checked(!s.sync_image && !s.sync_audio && s.mode == Some(5));
    panel.set_mode6_checked(!s.sync_image && !s.sync_audio && s.mode == Some(6));
    panel.set_brightness(s.brightness);
    panel.set_brightness_text(txt(&s.brightness_text));
    panel.set_rainbow(s.rainbow);
    panel.set_solid_color(s.solid);
    panel.set_solid_set(s.solid_set);
    panel.set_solid_text(txt(&s.solid_text));
    panel.set_autostart(s.autostart);

    let l = &s.labels;
    // The option models are built here, on the Slint thread: ModelRc is
    // not Send, so they cannot ride the snapshot (the labels do).
    panel.set_sample_options(string_model(&l.samples));
    panel.set_sample_index(s.sample_index);
    panel.set_smooth_options(string_model(&l.smooths));
    panel.set_smooth_index(s.smooth_index);
    panel.set_boost_options(string_model(&l.boosts));
    panel.set_boost_index(s.boost_index);
    panel.set_fps_options(string_model(&l.fpses));
    panel.set_fps_index(s.fps_index);
    panel.set_gain_options(string_model(&l.gains));
    panel.set_gain_index(s.gain_index);
    panel.set_lang_options(string_model(&l.langs));
    panel.set_lang_index(s.lang_index);
    let h = &l.hints;
    panel.set_hint_brightness(txt(h.brightness));
    panel.set_hint_sampling(txt(h.sampling));
    panel.set_hint_smoothing(txt(h.smoothing));
    panel.set_hint_boost(txt(h.boost));
    panel.set_hint_fps(txt(h.fps));
    panel.set_hint_static_color(txt(h.static_color));
    panel.set_hint_audio_color(txt(h.audio_color));
    panel.set_hint_sensitivity(txt(h.sensitivity));
    panel.set_hint_blink(txt(h.blink));
    panel.set_hint_range(txt(h.range));
    panel.set_blink_options(string_model(&l.blinks));
    panel.set_blink_index(s.blink_index);
    panel.set_range_options(string_model(&l.ranges));
    panel.set_range_index(s.range_index);
    panel.set_lbl_bright(txt(l.bright));
    panel.set_lbl_color(txt(l.color));
    panel.set_lbl_sampling(txt(l.sampling));
    panel.set_lbl_sens(txt(l.sens));
    panel.set_lbl_smooth(txt(l.smooth));
    panel.set_lbl_boost(txt(l.boost));
    panel.set_lbl_blink(txt(l.blink));
    panel.set_lbl_range(txt(l.range));
    panel.set_lbl_rainbow(txt(l.rainbow));
    panel.set_lbl_autostart(txt(l.autostart));
    panel.set_lbl_language(txt(l.language));
    panel.set_mode1_text(txt(&l.modes[0]));
    panel.set_mode2_text(txt(&l.modes[1]));
    panel.set_mode3_text(txt(&l.modes[2]));
    panel.set_mode4_text(txt(&l.modes[3]));
    panel.set_mode5_text(txt(&l.modes[4]));
    panel.set_mode6_text(txt(&l.modes[5]));
}

/// Native HWND of the panel window (0 while no window exists). Used by the
/// close path (instant native hide) and to re-focus an already-open panel.
static PANEL_HWND: AtomicIsize = AtomicIsize::new(0);
/// Set while a panel window exists: gates [`sync`] so a closed panel costs
/// nothing (no snapshots are built or shipped).
static WINDOW_OPEN: AtomicBool = AtomicBool::new(false);
/// Guards "the panel thread was spawned" (once per process).
static SPAWNED: AtomicBool = AtomicBool::new(false);
/// Wake-up channel into the panel thread: carries only the FIRST request
/// (which window to build before the event loop starts). Every later
/// request arrives through `invoke_from_event_loop`.
static WAKE: Mutex<Option<Sender<Wake>>> = Mutex::new(None);
/// The live window, as seen from other threads (`slint::Weak` is Send+Sync;
/// it upgrades only on the Slint thread, inside event-loop closures).
static CURRENT: Mutex<Option<slint::Weak<PanelWindow>>> = Mutex::new(None);
/// Monotonic open counter: a stale `Open` queued before a close must not
/// re-open a window the user just closed.
/// The window subclass state (all touched on the Slint thread only).
static ORIG_WNDPROC: AtomicIsize = AtomicIsize::new(0);
/// Current value of the window's `force-repaint` property; each repaint
/// trigger flips it, so consecutive triggers always change the value.
static REPAINT_FLIP: AtomicBool = AtomicBool::new(false);
/// The latest applied snapshot. The selector callbacks read it to keep pure
/// radio semantics (picking the active option is a no-op); written on the
/// Slint thread by [`sync`], read by widget callbacks on the same thread.
static LAST: Mutex<Option<Snapshot>> = Mutex::new(None);

/// The first request a freshly spawned panel thread serves before entering
/// the (never-returning) event loop.
enum Wake {
    Panel,
    Picker(PickerRequest),
}

/// Posted to the pump thread once a window is live, and on every reopen:
/// the snapshot pushed synchronously with the "open_panel" event is always
/// lost — no window exists to receive it yet — so without this the panel
/// would paint property defaults (empty labels).
pub const PANEL_READY_EVENT: &str = "__panel_ready";

/// Shows the panel: the first open spawns the panel thread (which builds
/// the window); an open with the window already up just re-centers and
/// focuses it; after a close, the parked thread is woken to build a fresh
/// window.
pub fn open(events: &Events) {
    start_panel_thread(events, Wake::Panel);
}

/// Opens the dark color picker for a static slot or the Audio Sync color,
/// replacing the light ChooseColor dialog. The result arrives on the pump
/// thread as a `__picker_*` event once the user accepts.
pub fn open_picker(events: &Events, request: PickerRequest) {
    start_panel_thread(events, Wake::Picker(request));
}

/// Wakes the panel thread for the first request of its life (channel handoff
/// before the event loop exists); every later request is queued straight
/// onto the running loop.
fn start_panel_thread(events: &Events, first: Wake) {
    if SPAWNED.swap(true, Ordering::SeqCst) {
        dispatch(events, first);
        return;
    }
    let (tx, rx) = std::sync::mpsc::channel::<Wake>();
    *WAKE.lock().unwrap() = Some(tx);
    let events = *events;
    std::thread::Builder::new()
        .name("panel".into())
        .spawn(move || panel_thread(events, rx))
        .expect("failed to spawn panel thread");
    if let Some(tx) = WAKE.lock().ok().and_then(|w| w.clone()) {
        let _ = tx.send(first);
    }
}

/// Queues a request onto the running event loop. The loop is guaranteed
/// live here: the thread only stays reachable through this path after it
/// has claimed the loop. (On the rare lost race between thread spawn and
/// loop start the request is dropped; the user's retry lands fine.)
fn dispatch(events: &Events, first: Wake) {
    let ev = *events;
    let _ = slint::invoke_from_event_loop(move || match first {
        Wake::Panel => open_panel_ui(&ev),
        Wake::Picker(request) => show_picker(&ev, request),
    });
}

/// Creates (or restores/focuses) the panel window. Runs on the Slint thread.
fn open_panel_ui(events: &Events) {
    use slint::ComponentHandle;
    if WINDOW_OPEN.load(Ordering::SeqCst) {
        let weak = CURRENT.lock().ok().and_then(|c| c.clone());
        if let Some(panel) = weak.and_then(|w| w.upgrade()) {
            let hwnd = PANEL_HWND.load(Ordering::SeqCst);
            if hwnd != 0 {
                use windows::Win32::Foundation::HWND;
                use windows::Win32::UI::WindowsAndMessaging::{IsIconic, ShowWindow, SW_RESTORE};
                let native = HWND(hwnd as *mut core::ffi::c_void);
                unsafe {
                    if IsIconic(native).as_bool() {
                        let _ = ShowWindow(native, SW_RESTORE);
                    }
                }
            }
            center_on_cursor(&panel);
            let _ = unsafe {
                windows::Win32::UI::WindowsAndMessaging::SetForegroundWindow(
                    windows::Win32::Foundation::HWND(hwnd as *mut core::ffi::c_void),
                )
            };
        }
        return;
    }
    let Some(panel) = build_window(events) else {
        return;
    };
    *CURRENT.lock().unwrap() = Some(panel.as_weak());
    WINDOW_OPEN.store(true, Ordering::SeqCst);
    PANEL_WIN.with(|w| *w.borrow_mut() = Some(panel));
    PANEL_WIN.with(|w| {
        if let Some(p) = w.borrow().as_ref() {
            p.show().ok();
            center_on_cursor(p);
        }
    });
    events.send(PANEL_READY_EVENT.into());
}

/// The panel thread: parked in `recv` while no window exists (zero CPU, no
/// renderer allocations); per open, builds a window, runs the Slint event
/// loop until the user closes it, then frees everything and parks again.
/// The thread (and the winit event loop it owns) lives for the process:
/// winit does not support creating a second event loop.
fn panel_thread(events: Events, wake: Receiver<Wake>) {
    use slint::ComponentHandle;

    // Serve the first request, then claim the event loop for the rest of
    // the process: winit does not support creating a second event loop, so
    // the thread never lets this one go. Later requests (open panel, pick
    // color, snapshots) arrive through `invoke_from_event_loop`; with
    // nothing on screen the loop idles in the OS message wait at zero CPU,
    // and every window is destroyed on close, so an idle tray holds no
    // renderer memory.
    match wake.recv() {
        Ok(Wake::Panel) => {
            let Some(panel) = build_window(&events) else {
                return;
            };
            *CURRENT.lock().unwrap() = Some(panel.as_weak());
            WINDOW_OPEN.store(true, Ordering::SeqCst);
            PANEL_WIN.with(|w| *w.borrow_mut() = Some(panel));
            PANEL_WIN.with(|w| {
                if let Some(p) = w.borrow().as_ref() {
                    p.show().ok();
                    center_on_cursor(p);
                }
            });

            // The window is live: ask the pump for the current state (the
            // snapshot sent with the "open_panel" event predates it).
            events.send(PANEL_READY_EVENT.into());
        }
        Ok(Wake::Picker(request)) => show_picker(&events, request),
        Err(_) => return,
    }

    let result = slint::run_event_loop_until_quit();
    if result.is_err() {
        eprintln!("panel: event loop error: {result:?}");
    }
}

// Strong handles of the live windows. The event loop never returns, so
// these live on the panel thread only; close paths take() and drop them,
// which destroys the window and frees its renderer memory in place.
thread_local! {
    static PANEL_WIN: RefCell<Option<PanelWindow>> = const { RefCell::new(None) };
    static PICKER_WIN: RefCell<Option<ColorPicker>> = const { RefCell::new(None) };
}

/// Deferred panel teardown (runs right after the close-request dispatch):
/// drops the strong handle — destroying the window and its renderer memory
/// — and returns the pages to the OS. The event loop keeps running for
/// later opens and pickers.
fn close_panel() {
    PANEL_HWND.store(0, Ordering::SeqCst);
    *CURRENT.lock().unwrap() = None;
    WINDOW_OPEN.store(false, Ordering::SeqCst);
    LAST.lock().unwrap().take();
    PANEL_WIN.with(|w| drop(w.borrow_mut().take()));
    ORIG_WNDPROC.store(0, Ordering::SeqCst);
    trim_working_set();
}

/// Builds the panel window with every callback registered. Returns None
/// when Slint cannot create the window (logged; the tray keeps working).
fn build_window(events: &Events) -> Option<PanelWindow> {
    use slint::{CloseRequestResponse, ComponentHandle};

    let panel = match PanelWindow::new() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("panel: slint window creation failed: {e}");
            return None;
        }
    };
    register_callbacks(&panel, *events);
    // Close: hide natively at once, then destroy the window (deferred past
    // this message's dispatch) and hand its memory back. The event loop
    // itself keeps running for later opens and pickers.
    panel.window().on_close_requested(|| {
        hide_native_now();
        slint::Timer::single_shot(Duration::ZERO, close_panel);
        CloseRequestResponse::KeepWindowShown
    });

    // The native window only exists once the event loop maps the pending
    // adapter (resumed), so the subclass is installed from inside the loop,
    // on the first tick after it starts. One single-shot timer, dropped
    // after (at most) a few retries — no recurring wakeups.
    let weak = panel.as_weak();
    let attempts = Rc::new(AtomicUsize::new(0));
    let timer = Rc::new(slint::Timer::default());
    {
        let weak = weak.clone();
        let timer_cb = timer.clone();
        let attempts_cb = attempts.clone();
        timer.start(
            slint::TimerMode::SingleShot,
            Duration::from_millis(30),
            move || install_subclass_when_ready(&weak, &timer_cb, &attempts_cb),
        );
    }
    Some(panel)
}

/// Tries to subclass the native window; retries a few times if the window
/// manager has not mapped it yet.
fn install_subclass_when_ready(
    weak: &slint::Weak<PanelWindow>,
    timer: &Rc<slint::Timer>,
    attempts: &Rc<AtomicUsize>,
) {
    let Some(panel) = weak.upgrade() else { return };
    match native_hwnd(&panel) {
        Some(hwnd) => {
            PANEL_HWND.store(hwnd, Ordering::SeqCst);
            install_subclass(hwnd);
        }
        None => {
            if attempts.fetch_add(1, Ordering::SeqCst) < 20 {
                let weak = weak.clone();
                let timer_cb = timer.clone();
                let attempts_cb = attempts.clone();
                timer.start(
                    slint::TimerMode::SingleShot,
                    Duration::from_millis(30),
                    move || install_subclass_when_ready(&weak, &timer_cb, &attempts_cb),
                );
            }
        }
    }
}

/// Native HWND of `panel`, once the window manager has created it.
fn native_hwnd(panel: &PanelWindow) -> Option<isize> {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    let wh = panel.window().window_handle();
    let handle = wh.window_handle().ok()?;
    match handle.as_raw() {
        RawWindowHandle::Win32(w) => Some(w.hwnd.get()),
        _ => None,
    }
}

/// Installs [panel_wndproc] ahead of winit's own procedure. Must run on the
/// window's thread; the original procedure is remembered in a static
/// because only one panel window ever exists at a time.
fn install_subclass(hwnd: isize) {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::{SetWindowLongPtrW, GWLP_WNDPROC};
    let h = HWND(hwnd as *mut core::ffi::c_void);
    let orig = unsafe { SetWindowLongPtrW(h, GWLP_WNDPROC, panel_wndproc as *const () as isize) };
    ORIG_WNDPROC.store(orig, Ordering::SeqCst);
    apply_resource_icons(hwnd);
}

/// Sets the window icons from the multi-size .ico embedded as resource ID 1
/// (the same artwork Explorer uses for the exe). Slint's `icon` decodes the
/// 512px PNG and downscales — blurry at 16px — while Windows picks the
/// purpose-drawn frame per size from the .ico. Small = title bar, big =
/// taskbar and alt-tab.
/// Native HWND of any Slint window (0 if the manager has not mapped it).
pub(crate) fn native_hwnd_of(window: &slint::Window) -> Option<isize> {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    let binding = window.window_handle();
    let handle = binding.window_handle().ok()?;
    match handle.as_raw() {
        RawWindowHandle::Win32(w) => Some(w.hwnd.get()),
        _ => None,
    }
}

pub(crate) fn apply_resource_icons(hwnd: isize) {
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{LPARAM, WPARAM};
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::WindowsAndMessaging::{
        LoadImageW, SendMessageW, ICON_BIG, ICON_SMALL, IMAGE_ICON, LR_DEFAULTCOLOR, WM_SETICON,
    };
    unsafe {
        let hinstance = GetModuleHandleW(None).unwrap_or_default();
        // MAKEINTRESOURCE(1): the resource id packed into a pointer, as Windows expects
        let name = PCWSTR(std::ptr::dangling::<u16>());
        for (size, flag) in [(32i32, ICON_BIG), (16i32, ICON_SMALL)] {
            if let Ok(handle) = LoadImageW(
                Some(hinstance.into()),
                name,
                IMAGE_ICON,
                size,
                size,
                LR_DEFAULTCOLOR,
            ) {
                let _ = SendMessageW(
                    windows::Win32::Foundation::HWND(hwnd as *mut core::ffi::c_void),
                    WM_SETICON,
                    Some(WPARAM(flag as usize)),
                    Some(LPARAM(handle.0 as isize)),
                );
            }
        }
    }
}

/// The repaint trigger: flips the window's `force-repaint` property, which
/// dirties the full-window overlay and forces the software renderer to blit
/// the entire window on its next frame. Must run on the Slint thread.
fn force_full_repaint() {
    let weak = CURRENT.lock().ok().and_then(|c| c.clone());
    let Some(weak) = weak else { return };
    let Some(panel) = weak.upgrade() else { return };
    let next = REPAINT_FLIP.fetch_xor(true, Ordering::SeqCst);
    panel.set_force_repaint(next);
}

type WndProcFn = unsafe extern "system" fn(
    windows::Win32::Foundation::HWND,
    u32,
    windows::Win32::Foundation::WPARAM,
    windows::Win32::Foundation::LPARAM,
) -> windows::Win32::Foundation::LRESULT;

/// Subclass procedure in front of winit's: sees the messages the window
/// receives before Slint does. `WM_PAINT` means the system invalidated the
/// window's surface (restore from minimized, uncovering after the surface
/// was discarded, …) and `WM_SIZE`/`SIZE_RESTORED` means the window just
/// came back from minimized — both are exactly the situations where the
/// renderer's dirty tracking may be empty while the on-screen surface was
/// recreated blank, so both force a full repaint.
unsafe extern "system" fn panel_wndproc(
    hwnd: windows::Win32::Foundation::HWND,
    msg: u32,
    wparam: windows::Win32::Foundation::WPARAM,
    lparam: windows::Win32::Foundation::LPARAM,
) -> windows::Win32::Foundation::LRESULT {
    use windows::Win32::Graphics::Gdi::GetUpdateRect;
    use windows::Win32::UI::WindowsAndMessaging::{
        CallWindowProcW, DefWindowProcW, SIZE_RESTORED, WM_PAINT, WM_SIZE,
    };
    match msg {
        // Flip only on a REAL paint: one whose update region is non-empty.
        // Winit schedules redraws by posting RDW_INTERNALPAINT, which also
        // arrives as WM_PAINT but with an empty update region — flipping on
        // those feeds back (flip -> request_redraw -> internal paint ->
        // flip …), rendering the full window at full speed for as long as
        // the panel is visible.
        WM_PAINT => {
            let mut rc = windows::Win32::Foundation::RECT::default();
            if GetUpdateRect(hwnd, Some(&mut rc), false).as_bool() {
                force_full_repaint();
            }
        }
        WM_SIZE if wparam.0 as u32 == SIZE_RESTORED => force_full_repaint(),
        _ => {}
    }
    let orig = ORIG_WNDPROC.load(Ordering::SeqCst);
    if orig == 0 {
        // Between installation and teardown the original is always stored;
        // this is unreachable belt-and-braces.
        return DefWindowProcW(hwnd, msg, wparam, lparam);
    }
    CallWindowProcW(
        Some(std::mem::transmute::<isize, WndProcFn>(orig)),
        hwnd,
        msg,
        wparam,
        lparam,
    )
}

/// Close-time native hide: instant disappearance on the user's click,
/// before the event loop unwinds. From here on the window is never shown
/// again (teardown destroys it), so Slint's hide/re-show quirks on this
/// backend never come into play.
fn hide_native_now() {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::{ShowWindow, SW_HIDE};
    let hwnd = PANEL_HWND.load(Ordering::SeqCst);
    if hwnd != 0 {
        let _ = unsafe { ShowWindow(HWND(hwnd as *mut core::ffi::c_void), SW_HIDE) };
    }
}

/// Full teardown of a closed panel: undo the subclass, forget the native
/// handle, drop the weak reference and destroy the window by dropping the
/// last strong handle.
/// Hands the process's physical pages back to the OS. The commit charge
/// stays reserved (pagefile-backed) until process exit.
pub(crate) fn trim_working_set() {
    unsafe {
        let _ = windows::Win32::System::Memory::SetProcessWorkingSetSizeEx(
            windows::Win32::System::Threading::GetCurrentProcess(),
            usize::MAX,
            usize::MAX,
            windows::Win32::System::Memory::SETPROCESSWORKINGSETSIZEEX_FLAGS(0),
        );
    }
}

/// Work-area center for `w` x `h` physical pixels on the monitor under the
/// cursor. Shared with the picker window.
pub(crate) fn cursor_monitor_center(w: i32, h: i32) -> Option<(i32, i32)> {
    use windows::Win32::Foundation::POINT;
    use windows::Win32::Graphics::Gdi::{
        GetMonitorInfoW, MonitorFromPoint, MONITORINFO, MONITOR_DEFAULTTONEAREST,
    };
    use windows::Win32::UI::WindowsAndMessaging::GetCursorPos;
    unsafe {
        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        let mut info = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if GetMonitorInfoW(MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST), &mut info).as_bool() {
            let x = info.rcWork.left + (info.rcWork.right - info.rcWork.left - w) / 2;
            let y = info.rcWork.top + (info.rcWork.bottom - info.rcWork.top - h) / 2;
            return Some((x, y));
        }
    }
    None
}

/// Centers the panel on the work area of the monitor under the cursor (the
/// one the user opened it from), like the color picker dialog. Must be
/// called after show(): it needs the window's scale factor.
fn center_on_cursor(panel: &PanelWindow) {
    use windows::Win32::Foundation::POINT;
    use windows::Win32::Graphics::Gdi::{
        GetMonitorInfoW, MonitorFromPoint, MONITORINFO, MONITOR_DEFAULTTONEAREST,
    };
    use windows::Win32::UI::WindowsAndMessaging::GetCursorPos;

    unsafe {
        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        let mut info = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if GetMonitorInfoW(MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST), &mut info).as_bool() {
            // Fixed logical size by design; the scale turns it physical.
            // Keep in sync with the Window's min==max size above.
            let scale = f64::from(panel.window().scale_factor());
            let w = (670.0 * scale) as i32;
            let h = (372.0 * scale) as i32;
            let x = info.rcWork.left + (info.rcWork.right - info.rcWork.left - w) / 2;
            let y = info.rcWork.top + (info.rcWork.bottom - info.rcWork.top - h) / 2;
            panel
                .window()
                .set_position(slint::PhysicalPosition::new(x, y));
        }
    }
}

/// Every widget callback becomes one of the menu's own event ids.
fn register_callbacks(panel: &PanelWindow, events: Events) {
    {
        let ev = events;
        panel.on_lighting_selected(move |which| {
            let last = LAST.lock().ok();
            let last = last.as_ref().and_then(|l| l.as_ref());
            match which {
                6 => {
                    let active = last.is_some_and(|s| s.sync_image);
                    if !active {
                        ev.send("toggle_image_sync".into());
                    }
                }
                7 => {
                    let active = last.is_some_and(|s| s.sync_audio);
                    if !active {
                        ev.send("toggle_audio".into());
                    }
                }
                i => {
                    let same = last.is_some_and(|s| {
                        !s.sync_image && !s.sync_audio && s.mode == Some(i as u8 + 1)
                    });
                    if !same {
                        ev.send(format!("mode_{}", i + 1));
                    }
                }
            }
        });
    }
    {
        let ev = events;
        panel.on_power_changed(move |on| {
            // The bool is the switch state AFTER the toggle.
            ev.send(if on { "on" } else { "off" }.into())
        });
    }
    {
        let ev = events;
        panel.on_brightness_changed(move |v| ev.send(format!("bright_{}", v as i32)));
    }
    {
        let ev = events;
        panel.on_smooth_select(move |i| ev.send(format!("smooth_{i}")));
    }
    {
        let ev = events;
        panel.on_boost_select(move |i| ev.send(format!("boost_{i}")));
    }
    {
        let ev = events;
        panel.on_fps_select(move |i| ev.send(format!("fps_{i}")));
    }
    {
        let ev = events;
        panel.on_gain_select(move |i| ev.send(format!("gain_{i}")));
    }
    {
        let ev = events;
        panel.on_sampling_select(move |i| {
            ev.send(
                match i {
                    0 => "sampling_5",
                    1 => "sampling_15",
                    _ => "sampling_full",
                }
                .into(),
            )
        });
    }
    {
        let ev = events;
        panel.on_slot_pick(move |slot| ev.send(format!("slotcolor_{slot}")));
    }
    {
        let ev = events;
        panel.on_rainbow_changed(move |on| {
            // The bool is the switch state AFTER the toggle: off must hand
            // the strip back to the remembered solid color, not re-arm the
            // sweep (the menu's own check carries the same toggle semantics
            // through muda's pre-flipped state).
            ev.send(
                if on {
                    "pv_audiorainbow_on"
                } else {
                    "audiocolor_solid_restore"
                }
                .into(),
            )
        });
    }
    {
        let ev = events;
        panel.on_solid_pick(move || ev.send("audiocolor_solid".into()));
    }
    {
        let ev = events;
        panel.on_blink_select(move |i| ev.send(format!("audioblink_{i}")));
    }
    {
        let ev = events;
        panel.on_range_select(move |i| ev.send(format!("audiorange_{i}")));
    }
    {
        let ev = events;
        panel.on_autostart_toggled(move || ev.send("pv_autostart".into()));
    }
    {
        let ev = events;
        panel.on_lang_select(move |value| {
            // The value is one of the same labels the tray menu shows; map
            // it back to the LANGUAGES index and reuse the menu's own id.
            if let Some(i) = language_labels().iter().position(|l| *l == value.as_str()) {
                ev.send(format!("lang_{i}"));
            }
        });
    }
}

/// Pushes the current state to the panel window. Called after each event;
/// a no-op while no window exists (the panel fully unloaded between
/// sessions). Each snapshot wakes the Slint event loop exactly once — no
/// polling timer runs while the window is idle.
pub fn sync(ui: &Ui) {
    if !WINDOW_OPEN.load(Ordering::SeqCst) {
        return;
    }
    let weak = match CURRENT.lock().ok().and_then(|c| c.clone()) {
        Some(w) => w,
        None => return,
    };
    let snapshot = snapshot_from(ui);
    let _ = slint::invoke_from_event_loop(move || {
        // The window may have closed between the check above and this
        // closure running; the weak handle simply fails to upgrade.
        *LAST.lock().unwrap() = Some(snapshot.clone());
        if let Some(panel) = weak.upgrade() {
            apply(&panel, &snapshot);
        }
    });
}
