//! Tray menu: item construction, state and event handling.
//!
//! [`Ui`] owns every check item plus the monitor state the checks reflect.
//! It is created once in `main` and moved into the message pump; every event
//! (menu ids and cross-thread notifications alike) funnels through
//! [`handle_event`], which mutates the state and lets [`sync_menu`] repaint
//! the checks.

use crate::audio::{AudioColor, Blink, DynamicRange};
use crate::engine::{
    AudioParams, Engine, ImageSyncParams, Source, AUDIO_SYNC_MODE, ENGINE_FAILED_EVENT,
    VIDEO_SYNC_MODE,
};
use crate::events::{self, Events};
use crate::i18n::{self, t, Lang, Language};
use crate::panel;
use crate::picker::PickerRequest;

use crate::sampling::SamplingMode;
use crate::settings::{self, Settings};
use crate::usb::{self, UsbCommand};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;
use tray_icon::menu::{
    CheckMenuItem, Icon, IconMenuItem, Menu, MenuItem, PredefinedMenuItem, Submenu,
};
use tray_icon::TrayIcon;

type Label = fn(&Lang) -> &'static str;

/// Delayed second pass of monitor-state recovery, one per monitor
/// (re)appearance: the display topology and the USB enumeration usually
/// report the monitor before its lighting MCU parses reports, so the first
/// restore burst can land on a device that is still booting.
const RECOVERY_REPEAT_DELAY: Duration = Duration::from_secs(1);

/// UserEvent for that second pass. The handler re-reads the current state,
/// so anything the user changed in the meantime wins.
const RESTORE_PUSH_EVENT: &str = "__restore_push";

pub(crate) const SMOOTHING: [(Label, f32); 4] = [
    (|l| l.smooth_instant, 0.0),
    (|l| l.smooth_low, 0.2),
    (|l| l.smooth_normal, 0.4),
    (|l| l.smooth_high, 0.6),
];

pub(crate) const BOOST: [(Label, f32); 4] = [
    (|_| "1.0x", 1.0),
    (|_| "1.2x", 1.2),
    (|_| "1.5x", 1.5),
    (|_| "2.0x", 2.0),
];

pub(crate) const FPS: [(Label, u32); 4] = [
    (|_| "10", 10),
    (|_| "15", 15),
    (|_| "30", 30),
    (|_| "60", 60),
];

/// Language menu entries: endonym labels (fixed per language), except the
/// System entry whose label follows the active language.
pub(crate) const LANGUAGES: [(&str, Language); 11] = [
    ("", Language::System),
    ("Español", Language::Es),
    ("English", Language::En),
    ("Français", Language::Fr),
    ("Deutsch", Language::De),
    ("Português", Language::Pt),
    ("Italiano", Language::It),
    ("Русский", Language::Ru),
    ("日本語", Language::Ja),
    ("中文", Language::Zh),
    ("한국어", Language::Ko),
];

pub(crate) const GAIN: [(Label, f32); 3] = [
    (|l| l.gain_low, 0.5),
    (|l| l.gain_medium, 1.0),
    (|l| l.gain_high, 2.0),
];
const DEFAULT_GAIN: f32 = 1.0;

const BLINK: [(Label, Blink); 3] = [
    (|l| l.blink_smooth, Blink::Smooth),
    (|l| l.blink_normal, Blink::Normal),
    (|l| l.blink_fast, Blink::Fast),
];

const RANGE: [(Label, DynamicRange); 4] = [
    (|l| l.range_compressed, DynamicRange::Compressed),
    (|l| l.range_normal, DynamicRange::Normal),
    (|l| l.range_expanded, DynamicRange::Expanded),
    (|l| l.range_extreme, DynamicRange::Extreme),
];

/// Which sync source is running. Both funnel through the event-loop thread,
/// so a plain field stays consistent.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum SyncActive {
    None,
    ImageSync,
    Audio,
}

/// Check items for a preset group, paired with the value each item stands for.
pub type PresetChecks<T> = Vec<(T, CheckMenuItem)>;

/// Mutable menu state, owned by the event-loop closure.
pub struct Ui {
    pub tray: Option<TrayIcon>,
    /// Current sync source. All starts/stops funnel through the event-loop
    /// thread, so a plain field stays consistent.
    pub(crate) sync: SyncActive,
    toggle_image_sync: CheckMenuItem,
    toggle_audio: CheckMenuItem,
    /// One check item per brightness level (1..=12).
    bright_items: PresetChecks<u8>,
    /// One check item per device mode (1..=6; modes 7/8 are the sync modes,
    /// driven by Audio Sync / Image Sync).
    mode_items: PresetChecks<u8>,
    /// One picker entry per static slot; its label shows the current hex and
    /// its icon a swatch of the color.
    slot_color_items: Vec<(u8, IconMenuItem)>,
    /// Audio color entries: the rainbow toggle and the solid-color picker.
    audio_rainbow_item: CheckMenuItem,
    audio_solid_item: IconMenuItem,
    /// One check item per sampling mode (5% ring / 15% ring / full screen).
    sampling_items: PresetChecks<SamplingMode>,
    /// One check item per smoothing / boost / fps / gain preset.
    smooth_items: PresetChecks<f32>,
    boost_items: PresetChecks<f32>,
    fps_items: PresetChecks<u32>,
    gain_items: PresetChecks<f32>,
    /// One check item per audio blink / dynamic-range preset.
    audio_blink_items: PresetChecks<Blink>,
    audio_range_items: PresetChecks<DynamicRange>,
    /// One check item per menu language (switching rebuilds the menu live).
    language_items: PresetChecks<Language>,
    autostart_item: CheckMenuItem,
    /// Desktop output of the RGB-strip monitor (the image sync source),
    /// re-detected on every topology change. Empty = the monitor's desktop
    /// is not there right now (standby, input switch).
    selected_screen: String,
    /// Sync to resume on the next "turn LEDs on", if the last lighting mode
    /// was a sync and no static mode has been chosen since. Also drives the
    /// panel view while the LEDs are off: it IS the active configuration.
    pub(crate) resume: Option<SyncActive>,
    /// Image Sync that was running at exit but whose screen was not up yet at
    /// app start (boot race): started on the first topology event reporting
    /// the RGB monitor's output back, or by the next "turn LEDs on". Cleared
    /// by any explicit user choice (static mode, power off, another sync).
    /// Also carries a sync picked while the LEDs are off, waiting for power.
    pub(crate) pending_boot_sync: Option<SyncActive>,
    /// Image Sync tuning, handed to the engine on every (re)start.
    pub(crate) params: ImageSyncParams,
    /// Audio Sync tuning, applied live through the engine's shared slot.
    pub(crate) audio: AudioParams,
    /// Last solid audio color picked, remembered so the rainbow sweep can
    /// be switched off again (None = never picked; restore falls back to
    /// white). Persisted alongside the color mode.
    pub(crate) audio_last_solid: Option<[u8; 3]>,
    /// Active UI language (switching applies live everywhere).
    pub(crate) language: Language,
    /// Monitor state as last pushed by this app: brightness, device mode and
    /// each static slot's color. None = never pushed this session (the
    /// protocol has no read-back), so nothing is checked until we act.
    pub(crate) brightness: Option<u8>,
    pub(crate) mode: Option<u8>,
    pub(crate) slot_colors: [Option<[u8; 3]>; 4],
    /// Last diagnostic line (missing screen, engine failure). Overrides the
    /// derived status in the panel until the next state change clears it —
    /// the same stickiness the menu status item has.
    pub(crate) note: Option<String>,
    /// Autostart registry state, kept in sync by both toggle surfaces.
    pub(crate) autostart: bool,
    /// Whether the strip is lit: the last power action said on (or any
    /// mode/color command lit it) and no power-off has followed. Session
    /// only — every launch restores the saved mode, which lights the strip.
    pub(crate) leds_on: bool,
    pub connected: Arc<AtomicBool>,
    engine: Arc<Engine>,
    /// Sender for the delayed recovery pass (see schedule_recovery_repeat).
    events: Events,
    /// True while a delayed recovery pass is pending: schedule_recovery_repeat
    /// drops duplicates so two sources racing at startup cannot double-fire.
    recovery_scheduled: bool,
    /// Disabled item showing the last notable state/diagnostic.
    status_item: MenuItem,
    /// The Mode submenu; its label names the active mode ("Mode - Static 2")
    /// and is repainted by sync_menu through the shared-state clone.
    mode_sub: Submenu,
    /// Last painted swatch state, so sync_menu skips redundant icon rebuilds.
    painted_slots: [Option<Option<[u8; 3]>>; 4],
    painted_audio_solid: Option<Option<[u8; 3]>>,
}

/// Creates the Ui state and every check item in it, seeded from the persisted
/// settings: checks reflect the last session, the saved monitor state is
/// reapplied to the device, and a running engine is resumed. The USB thread
/// must already be spawned (the restore commands travel its channel).
pub fn build_ui(
    engine: Arc<Engine>,
    connected: Arc<AtomicBool>,
    settings: Settings,
    events: Events,
) -> Ui {
    // Like LG's own software, image sync has no screen picker: the source is
    // the UltraGear display carrying the RGB strip, matched by EDID model and
    // re-detected on every topology change. Empty = the monitor's desktop is
    // not on the desktop right now.
    let selected_screen = crate::capture::find_lg_output()
        .map(|o| o.name)
        .unwrap_or_default();

    let mut params = ImageSyncParams::default();
    if let Some(v) = settings.sampling {
        params.sampling = v;
    }
    if let Some(v) = settings.smoothing {
        params.smoothing = v;
    }
    if let Some(v) = settings.boost {
        params.boost = v;
    }
    if let Some(v) = settings.fps {
        params.fps = v;
    }
    engine.set_image_params(params);
    let audio = AudioParams {
        gain: settings.audio_gain.unwrap_or(DEFAULT_GAIN),
        color: settings.audio_color.unwrap_or(AudioColor::Rainbow),
        blink: settings.audio_blink.unwrap_or(Blink::Normal),
        range: settings.audio_range.unwrap_or(DynamicRange::Normal),
    };
    let audio_last_solid = settings.audio_solid.or(match audio.color {
        AudioColor::Solid(rgb) => Some(rgb),
        AudioColor::Rainbow => None,
    });
    engine.set_audio_params(audio);
    engine.set_brightness(settings.brightness.unwrap_or(12));
    let (sampling, smoothing, boost, fps) =
        (params.sampling, params.smoothing, params.boost, params.fps);
    let ui_language = settings.language.unwrap_or(Language::System);

    // Every check item is created up front and owned by Ui; build_menu appends
    // clones of them (muda clones share state, so set_checked on our copy
    // reaches the menu). Initial checks come from the loaded settings.
    let mut bright_items: PresetChecks<u8> = Vec::new();
    for i in 1..=12u8 {
        bright_items.push((
            i,
            CheckMenuItem::with_id(
                format!("bright_{i}"),
                format!("{} {i}", t().level),
                true,
                settings.brightness == Some(i),
                None,
            ),
        ));
    }

    let mut mode_items: PresetChecks<u8> = Vec::new();
    for mode in 1..=6u8 {
        // Statics get a translated name; Peaceful/Dynamic keep LG's own names.
        mode_items.push((
            mode,
            CheckMenuItem::with_id(
                format!("mode_{mode}"),
                static_mode_name(mode),
                true,
                settings.mode == Some(mode),
                None,
            ),
        ));
    }

    let mut slot_color_items: Vec<(u8, IconMenuItem)> = Vec::new();
    for slot in 1..=4u8 {
        let color = settings.slot_colors[(slot - 1) as usize];
        slot_color_items.push((
            slot,
            IconMenuItem::with_id(
                format!("slotcolor_{slot}"),
                format!("Slot {slot}"),
                true,
                color.map(swatch_icon),
                None,
            ),
        ));
    }

    let mut sampling_items: PresetChecks<SamplingMode> = Vec::new();
    for (mode, key, label) in [
        (
            SamplingMode::Border5,
            "sampling_5",
            (|l: &Lang| l.border5) as Label,
        ),
        (
            SamplingMode::Border15,
            "sampling_15",
            (|l: &Lang| l.border15) as Label,
        ),
        (
            SamplingMode::Full,
            "sampling_full",
            (|l: &Lang| l.full_screen) as Label,
        ),
    ] {
        sampling_items.push((
            mode,
            CheckMenuItem::with_id(key, label(t()), true, mode == sampling, None),
        ));
    }

    let smooth_items: PresetChecks<f32> = build_presets(&SMOOTHING, "smooth", smoothing);
    let boost_items: PresetChecks<f32> = build_presets(&BOOST, "boost", boost);
    let fps_items: PresetChecks<u32> = build_presets(&FPS, "fps", fps);
    let gain_items: PresetChecks<f32> = build_presets(&GAIN, "gain", audio.gain);
    let audio_rainbow_item = CheckMenuItem::with_id(
        "audiocolor_rainbow",
        t().rainbow,
        true,
        audio.color == AudioColor::Rainbow,
        None,
    );
    // The solid-color entry only carries a swatch when a solid color is
    // actually set; while the rainbow sweep runs there is nothing to show
    // (the Arcoíris check already tells that story).
    let audio_solid_item = IconMenuItem::with_id(
        "audiocolor_solid",
        t().solid_color,
        true,
        match audio.color {
            AudioColor::Solid(rgb) => Some(swatch_icon(rgb)),
            AudioColor::Rainbow => None,
        },
        None,
    );
    let audio_blink_items: PresetChecks<Blink> = build_presets(&BLINK, "audioblink", audio.blink);
    let audio_range_items: PresetChecks<DynamicRange> =
        build_presets(&RANGE, "audiorange", audio.range);
    let mut language_items: PresetChecks<Language> = Vec::new();
    for (i, (label, lang)) in LANGUAGES.iter().enumerate() {
        let text = if *lang == Language::System {
            t().system
        } else {
            *label
        };
        language_items.push((
            *lang,
            CheckMenuItem::with_id(format!("lang_{i}"), text, true, *lang == ui_language, None),
        ));
    }

    // Seeded from the same flag the tray tooltip uses. If the USB worker's
    // initial enumeration is still running at this point this reads a stale
    // false; the worker's CONNECTION_INIT_EVENT repaints status and tooltip
    // the moment enumeration completes.
    let status_item = MenuItem::with_id(
        "__status",
        if connected.load(Ordering::SeqCst) {
            t().connected
        } else {
            t().disconnected
        },
        false,
        None,
    );

    // The Mode submenu's label names the active lighting mode ("Mode -
    // Static 2"); sync_menu repaints it on every event.
    let initial_sync = if settings.image_sync_running {
        SyncActive::ImageSync
    } else if settings.audio_running {
        SyncActive::Audio
    } else {
        SyncActive::None
    };
    let mode_sub = Submenu::new(mode_label(initial_sync, settings.mode), true);

    let mut ui = Ui {
        tray: None,
        sync: initial_sync,
        toggle_image_sync: CheckMenuItem::with_id(
            "toggle_image_sync",
            "Image Sync",
            true,
            settings.image_sync_running,
            None,
        ),
        toggle_audio: CheckMenuItem::with_id(
            "toggle_audio",
            "Audio Sync",
            true,
            settings.audio_running,
            None,
        ),
        bright_items,
        mode_items,
        slot_color_items,
        audio_rainbow_item,
        audio_solid_item,
        sampling_items,
        smooth_items,
        boost_items,
        fps_items,
        gain_items,
        audio_blink_items,
        audio_range_items,
        language_items,
        autostart_item: CheckMenuItem::with_id(
            "autostart",
            t().autostart,
            true,
            settings::autostart_enabled(),
            None,
        ),
        selected_screen,
        params: ImageSyncParams {
            sampling,
            smoothing,
            boost,
            fps,
        },
        audio,
        audio_last_solid,
        language: ui_language,
        note: None,
        autostart: settings::autostart_enabled(),
        // The startup restore re-applies the saved mode, which lights the
        // strip: the session starts (and the switch shows it) powered on.
        leds_on: true,
        brightness: settings.brightness,
        mode: settings.mode,
        slot_colors: settings.slot_colors,
        connected,
        engine,
        events,
        recovery_scheduled: false,
        resume: None,
        pending_boot_sync: None,
        status_item,
        mode_sub,
        painted_slots: settings.slot_colors.map(Some),
        painted_audio_solid: Some(match audio.color {
            AudioColor::Solid(rgb) => Some(rgb),
            AudioColor::Rainbow => None,
        }),
    };

    // Reapply the saved monitor state. Sync modes 7/8 are skipped: only
    // meaningful behind a running engine, whose init re-sends them below.
    push_device_state(&ui);
    // The monitor can be enumerated while its lighting MCU is still booting
    // (hard power cut, monitor waking from standby): the delayed pass
    // re-asserts the state if the burst above landed on a deaf device. Gated
    // on `connected` so a healthy resumed sync is spared a spurious
    // controlled reinit; when enumeration publishes later, the
    // CONNECTION_INIT handler schedules the pass instead (idempotent guard),
    // so the race between enumeration and this check is covered either way.
    if ui.connected.load(Ordering::SeqCst) {
        schedule_recovery_repeat(&mut ui);
    }

    // Resume image sync or audio if either was running at exit. Their init
    // lands after the restore commands on the control channel, overriding
    // them — mirror that in the state like the toggle handlers do.
    if settings.image_sync_running {
        if ui.selected_screen.is_empty() {
            // No RGB-strip monitor on the desktop (typically a boot race):
            // don't arm the device with no source behind it, and don't send
            // its mode 8 either (excluded below). Remember the intent — the
            // sync starts on the first topology event reporting the monitor
            // back (recover_monitor), or on the next "turn LEDs on".
            ui.sync = SyncActive::None;
            ui.pending_boot_sync = Some(SyncActive::ImageSync);
        } else {
            start_sync(&mut ui, SyncActive::ImageSync);
        }
    } else if settings.audio_running {
        start_sync(&mut ui, SyncActive::Audio);
    }

    ui
}

/// Recreates every language-dependent menu item from the current state, so
/// a live language switch can rebuild the tray menu without restarting.
/// Fixed-text items (the sync toggles) stay as they are, and the sync_menu
/// diff fields are refreshed to match the fresh icons, so the following
/// sync_menu call has nothing stale to repaint. Check states come from
/// `ui`, which mirrors the live state after every event.
fn recreate_items(ui: &mut Ui) {
    let mut bright_items: PresetChecks<u8> = Vec::new();
    for i in 1..=12u8 {
        bright_items.push((
            i,
            CheckMenuItem::with_id(
                format!("bright_{i}"),
                format!("{} {i}", t().level),
                true,
                ui.brightness == Some(i),
                None,
            ),
        ));
    }

    let mut mode_items: PresetChecks<u8> = Vec::new();
    for mode in 1..=6u8 {
        mode_items.push((
            mode,
            CheckMenuItem::with_id(
                format!("mode_{mode}"),
                static_mode_name(mode),
                true,
                ui.mode == Some(mode),
                None,
            ),
        ));
    }

    let mut slot_color_items: Vec<(u8, IconMenuItem)> = Vec::new();
    for slot in 1..=4u8 {
        slot_color_items.push((
            slot,
            IconMenuItem::with_id(
                format!("slotcolor_{slot}"),
                format!("Slot {slot}"),
                true,
                ui.slot_colors[(slot - 1) as usize].map(swatch_icon),
                None,
            ),
        ));
    }

    let mut sampling_items: PresetChecks<SamplingMode> = Vec::new();
    for (mode, key, label) in [
        (
            SamplingMode::Border5,
            "sampling_5",
            (|l: &Lang| l.border5) as Label,
        ),
        (
            SamplingMode::Border15,
            "sampling_15",
            (|l: &Lang| l.border15) as Label,
        ),
        (
            SamplingMode::Full,
            "sampling_full",
            (|l: &Lang| l.full_screen) as Label,
        ),
    ] {
        sampling_items.push((
            mode,
            CheckMenuItem::with_id(key, label(t()), true, mode == ui.params.sampling, None),
        ));
    }

    let smooth_items: PresetChecks<f32> = build_presets(&SMOOTHING, "smooth", ui.params.smoothing);
    let boost_items: PresetChecks<f32> = build_presets(&BOOST, "boost", ui.params.boost);
    let fps_items: PresetChecks<u32> = build_presets(&FPS, "fps", ui.params.fps);
    let gain_items: PresetChecks<f32> = build_presets(&GAIN, "gain", ui.audio.gain);
    let audio_rainbow_item = CheckMenuItem::with_id(
        "audiocolor_rainbow",
        t().rainbow,
        true,
        ui.audio.color == AudioColor::Rainbow,
        None,
    );
    let audio_solid_item = IconMenuItem::with_id(
        "audiocolor_solid",
        t().solid_color,
        true,
        match ui.audio.color {
            AudioColor::Solid(rgb) => Some(swatch_icon(rgb)),
            AudioColor::Rainbow => None,
        },
        None,
    );
    let audio_blink_items: PresetChecks<Blink> =
        build_presets(&BLINK, "audioblink", ui.audio.blink);
    let audio_range_items: PresetChecks<DynamicRange> =
        build_presets(&RANGE, "audiorange", ui.audio.range);
    let mut language_items: PresetChecks<Language> = Vec::new();
    for (i, (label, lang)) in LANGUAGES.iter().enumerate() {
        let text = if *lang == Language::System {
            t().system
        } else {
            *label
        };
        language_items.push((
            *lang,
            CheckMenuItem::with_id(format!("lang_{i}"), text, true, *lang == ui.language, None),
        ));
    }

    let status_item = MenuItem::with_id(
        "__status",
        if ui.connected.load(Ordering::SeqCst) {
            t().connected
        } else {
            t().disconnected
        },
        false,
        None,
    );
    let mode_sub = Submenu::new(mode_label(ui.sync, ui.mode), true);
    let autostart_item =
        CheckMenuItem::with_id("autostart", t().autostart, true, ui.autostart, None);

    ui.bright_items = bright_items;
    ui.mode_items = mode_items;
    ui.slot_color_items = slot_color_items;
    ui.sampling_items = sampling_items;
    ui.smooth_items = smooth_items;
    ui.boost_items = boost_items;
    ui.fps_items = fps_items;
    ui.gain_items = gain_items;
    ui.audio_rainbow_item = audio_rainbow_item;
    ui.audio_solid_item = audio_solid_item;
    ui.audio_blink_items = audio_blink_items;
    ui.audio_range_items = audio_range_items;
    ui.language_items = language_items;
    ui.status_item = status_item;
    ui.mode_sub = mode_sub;
    ui.autostart_item = autostart_item;
    // The fresh items already carry the current icons: mark the paint
    // trackers as up to date so sync_menu has nothing stale to redo.
    ui.painted_slots = ui.slot_colors.map(Some);
    ui.painted_audio_solid = Some(match ui.audio.color {
        AudioColor::Solid(rgb) => Some(rgb),
        AudioColor::Rainbow => None,
    });
}

/// Submenu with one appended check item per preset entry.
fn preset_sub(label: &str, items: &PresetChecks<impl Copy>) -> Submenu {
    let sub = Submenu::new(label, true);
    for (_, item) in items {
        sub.append(item).unwrap();
    }
    sub
}

/// Builds the whole menu tree by appending clones of the check items already
/// owned by `ui` (muda clones share their state, so the Ui copies control the
/// menu visuals).
pub fn build_menu(ui: &Ui) -> Menu {
    let menu = Menu::new();
    menu.append(&MenuItem::with_id("open_panel", t().open_panel, true, None))
        .unwrap();
    menu.append(&PredefinedMenuItem::separator()).unwrap();
    menu.append(&MenuItem::with_id("on", t().on_leds, true, None))
        .unwrap();
    menu.append(&MenuItem::with_id("off", t().off_leds, true, None))
        .unwrap();
    menu.append(&PredefinedMenuItem::separator()).unwrap();

    // Mode: the six device modes, then the two sync modes as toggles. The
    // submenu was seeded (empty) in build_ui; its label names the active mode
    // and is repainted by sync_menu.
    for (_, item) in &ui.mode_items {
        ui.mode_sub.append(item).unwrap();
    }
    ui.mode_sub
        .append(&PredefinedMenuItem::separator())
        .unwrap();
    ui.mode_sub.append(&ui.toggle_image_sync).unwrap();
    ui.mode_sub.append(&ui.toggle_audio).unwrap();
    menu.append(&ui.mode_sub).unwrap();

    menu.append(&PredefinedMenuItem::separator()).unwrap();

    // Settings: device brightness, then one config section per feature. The
    // screen picker lives inside Image Sync's section.
    let settings_sub = Submenu::new(t().settings, true);
    settings_sub
        .append(&preset_sub(t().brightness, &ui.bright_items))
        .unwrap();
    settings_sub
        .append(&PredefinedMenuItem::separator())
        .unwrap();
    let color_sub = Submenu::new(t().color, true);
    for (_, item) in &ui.slot_color_items {
        color_sub.append(item).unwrap();
    }
    settings_sub.append(&color_sub).unwrap();

    let image_sub = Submenu::new("Image Sync", true);
    image_sub
        .append(&preset_sub(t().sampling, &ui.sampling_items))
        .unwrap();
    image_sub
        .append(&preset_sub(t().smoothing, &ui.smooth_items))
        .unwrap();
    image_sub
        .append(&preset_sub(t().boost, &ui.boost_items))
        .unwrap();
    image_sub.append(&preset_sub("FPS", &ui.fps_items)).unwrap();
    settings_sub.append(&image_sub).unwrap();

    let audio_sub = Submenu::new("Audio Sync", true);
    audio_sub
        .append(&preset_sub(t().sensitivity, &ui.gain_items))
        .unwrap();
    let acolor_sub = Submenu::new(t().color, true);
    acolor_sub.append(&ui.audio_rainbow_item).unwrap();
    acolor_sub.append(&ui.audio_solid_item).unwrap();
    audio_sub.append(&acolor_sub).unwrap();
    audio_sub
        .append(&preset_sub(t().blink, &ui.audio_blink_items))
        .unwrap();
    audio_sub
        .append(&preset_sub(t().dynamic_range, &ui.audio_range_items))
        .unwrap();
    settings_sub.append(&audio_sub).unwrap();

    menu.append(&settings_sub).unwrap();
    menu.append(&PredefinedMenuItem::separator()).unwrap();
    menu.append(&ui.autostart_item).unwrap();
    menu.append(&preset_sub(t().language, &ui.language_items))
        .unwrap();
    // Status line: the disabled diagnostic item whose text the handlers
    // maintain (connection state, missing screen, engine failure reasons).
    menu.append(&PredefinedMenuItem::separator()).unwrap();
    menu.append(&ui.status_item).unwrap();
    menu.append(&PredefinedMenuItem::separator()).unwrap();
    menu.append(&MenuItem::with_id("quit", t().quit, true, None))
        .unwrap();
    menu
}

/// The derived status line: connection first, then what the app is doing.
/// Shared by the tray surfaces and the panel; the panel additionally shows
/// [`Ui::note`] when a diagnostic is sticking.
pub(crate) fn status_text(ui: &Ui) -> String {
    if !ui.connected.load(Ordering::SeqCst) {
        t().disconnected.to_string()
    } else {
        match ui.sync {
            SyncActive::ImageSync => "Image Sync".to_string(),
            SyncActive::Audio => "Audio Sync".to_string(),
            SyncActive::None => t().connected.to_string(),
        }
    }
}

/// Recomputes the two glanceable surfaces (status line + tray tooltip) from
/// the current state: connection first, then what the app is doing. Call on
/// every state change that can affect them. No device traffic: safe at any
/// moment, startup included. Deliberately NOT called after an engine-failure
/// event — that text must stick until the user acts on it.
pub fn refresh_status(ui: &mut Ui) {
    let state = status_text(ui);
    // A state change supersedes any sticking diagnostic.
    ui.note = None;
    let tooltip = format!("LG UltraGear RGB Control — {state}");
    if let Some(tray) = &ui.tray {
        let _ = tray.set_tooltip(Some(tooltip));
    }
    ui.status_item.set_text(state);
}

pub fn handle_event(id: String, ui: &mut Ui, quit: &mut bool) {
    match id.as_str() {
        usb::CONNECTION_EVENT => {
            refresh_status(ui);
            // The monitor just appeared with factory state, and restore
            // commands sent while it was absent were discarded by the USB
            // worker. Re-push what this app last set — or, with the LEDs
            // off, undo the factory lit state the MCU booted into (sync
            // modes 7/8 skipped: a running engine re-arms those on
            // reconnect).
            if ui.connected.load(Ordering::SeqCst) {
                if ui.leds_on {
                    push_device_state(ui);
                } else {
                    ui.engine.send(UsbCommand::TurnOff);
                }
                // The reconnection burst can also land on a lighting MCU that
                // is still booting: schedule the delayed pass.
                schedule_recovery_repeat(ui);
            }
        }

        usb::CONNECTION_INIT_EVENT => {
            // The worker's first enumeration finished after the UI seeded its
            // texts (possibly stale-false): repaint only. The restore burst
            // is already enqueued by build_ui, and a reconnect push here
            // would drag a resuming sync out of its mode — but the delayed
            // recovery pass DOES need scheduling here: build_ui's gate read
            // `connected` before this enumeration published it, so in launches
            // that lost that race the still-booting MCU would otherwise never
            // be re-asserted. The guard deduplicates against build_ui's own
            // schedule when enumeration won that race.
            refresh_status(ui);
            schedule_recovery_repeat(ui);
        }

        // The settings-save debounce fired: persist the current state. The
        // persist tail classifies this id as Skip, so this arm is the only
        // write for a debounced change.
        events::SETTINGS_FLUSH_EVENT => {
            settings::save(&current_settings(ui));
        }

        events::CHANGED_EVENT => redetect_screen(ui),

        // An HID device interface arrived or left (keyboard and mice count
        // too): the worker re-checks monitor presence right away instead of
        // waiting out its poll cadence.
        events::DEVICE_EVENT => {
            ui.engine.send(UsbCommand::Probe);
        }

        // One-shot delayed recovery pass (see RECOVERY_REPEAT_DELAY): the
        // first burst after a monitor (re)appearance or app start can land
        // while the lighting MCU is still booting. Reading the state NOW
        // means anything the user changed in the meantime wins.
        RESTORE_PUSH_EVENT => {
            // Consumed: later sources may schedule a fresh pass.
            ui.recovery_scheduled = false;
            if ui.connected.load(Ordering::SeqCst) {
                recover_monitor(ui);
            }
        }

        events::RESUME_EVENT => {
            // Sleep/resume re-trains the monitor's link and can reset the
            // lighting MCU with no USB re-enumeration and no topology change:
            // the delayed pass re-asserts the app's state once it is back.
            schedule_recovery_repeat(ui);
        }

        "toggle_image_sync" => {
            if ui.sync == SyncActive::ImageSync {
                stop_engine(ui, true);
            } else if !ui.leds_on {
                // The strip is dark: record the intent and wait. It starts
                // (lit) at the next power-on, like the boot-pending sync.
                ui.engine.stop();
                ui.resume = None;
                ui.pending_boot_sync = Some(SyncActive::ImageSync);
                ui.mode = Some(VIDEO_SYNC_MODE);
            } else if ui.selected_screen.is_empty() {
                // No RGB-strip monitor on the desktop: nothing to sample.
                ui.status_item.set_text(t().no_screen);
                ui.note = Some(t().no_screen.to_string());
            } else {
                ui.engine.stop();
                start_sync(ui, SyncActive::ImageSync);
            }
        }

        "toggle_audio" => {
            if ui.sync == SyncActive::Audio {
                stop_engine(ui, true);
            } else if !ui.leds_on {
                // Same deferred start as image sync.
                ui.engine.stop();
                ui.resume = None;
                ui.pending_boot_sync = Some(SyncActive::Audio);
                ui.mode = Some(AUDIO_SYNC_MODE);
            } else {
                ui.engine.stop();
                start_sync(ui, SyncActive::Audio);
            }
        }

        // The panel window: created lazily on first open, merely shown after.
        "open_panel" => panel::open(&ui.events),

        // The panel thread can receive snapshots now (first open) or asked
        // for a refresh (reopen). No state changes here: the common tail
        // below is what pushes the fresh snapshot.
        panel::PANEL_READY_EVENT => {}

        // The panel's autostart checkbox. The menu item arm reads muda's
        // auto-toggled check as its signal; from the panel that item was not
        // clicked, so toggle from the tracked state instead.
        "pv_autostart" => {
            let enable = !ui.autostart;
            if settings::autostart_set(enable) {
                ui.autostart = enable;
                ui.autostart_item.set_checked(enable);
            }
        }

        "quit" => {
            ui.engine.stop();
            // Ordered restore+Stop: control sends are FIFO per sender and
            // never block, so the whole sequence leaves the UI thread in
            // order with nothing to spawn. Mode before brightness,
            // consistent with the other disarm sequences (brightness lands
            // even while armed — hardware-verified).
            let level = ui.brightness.unwrap_or(12);
            // Keep an explicitly selected static mode across exit; only sync
            // modes (and a never-set state) fall back to Static 1 to disarm.
            let restore_mode = match ui.mode {
                Some(m) if (1..=6).contains(&m) => m,
                _ => 1,
            };
            ui.engine.send(UsbCommand::SetMode(restore_mode));
            ui.engine.send(UsbCommand::SetBrightness(level));
            ui.engine.send(UsbCommand::Stop);
            *quit = true;
        }

        _ => {
            if let Some(detail) = id.strip_prefix(ENGINE_FAILED_EVENT) {
                // Structured payload "<kind>;<detail>": the sentence is
                // localized here; the detail is the raw error text.
                let (kind, err) = detail.split_once(';').unwrap_or(("", detail));
                let text = match kind {
                    "image" => t().fail_image.replace("{0}", err),
                    "audio" => t().fail_audio.replace("{0}", err),
                    _ => detail.to_string(),
                };
                ui.status_item.set_text(text.clone());
                ui.note = Some(text);
                // The engine gave up on its own (terminal GPU or audio
                // error): disarm the monitor — it is still armed at sync
                // brightness and would sit there through the ~12 s sync
                // timeout. Mode first, then brightness: the monitor ignores
                // brightness while it stays armed (same order as
                // stop_engine).
                ui.resume = None;
                // Ordered control sends straight from the UI thread: FIFO
                // per sender, never blocking. Mode first, then brightness —
                // the same disarm sequence as stop_engine
                // (hardware-verified: brightness commands land even while
                // the monitor stays armed in a sync mode, so the order is
                // consistency rather than a requirement).
                let level = ui.brightness.unwrap_or(12);
                ui.engine.send(UsbCommand::SetMode(1));
                ui.engine.send(UsbCommand::SetBrightness(level));
                ui.mode = Some(1);
                // The disarm burst leaves the strip lit on Static 1.
                ui.leds_on = true;
                // Uncheck so the menu matches reality and a retry from the
                // menu is possible.
                ui.sync = SyncActive::None;
            } else if let Some(mode) = parse_sampling_mode(&id) {
                tweak(ui, ui.params.sampling == mode, |p| p.sampling = mode);
            } else if let Some(v) = parse_preset(&id, "smooth_", &SMOOTHING) {
                tweak(ui, ui.params.smoothing == v, |p| p.smoothing = v);
            } else if let Some(v) = parse_preset(&id, "boost_", &BOOST) {
                tweak(ui, ui.params.boost == v, |p| p.boost = v);
            } else if let Some(v) = parse_preset(&id, "fps_", &FPS) {
                tweak(ui, ui.params.fps == v, |p| p.fps = v);
            } else if let Some(v) = parse_preset(&id, "gain_", &GAIN) {
                tweak_audio(ui, ui.audio.gain == v, |p| p.gain = v);
            } else if let Some(v) = parse_preset(&id, "audioblink_", &BLINK) {
                tweak_audio(ui, ui.audio.blink == v, |p| p.blink = v);
            } else if let Some(v) = parse_preset(&id, "audiorange_", &RANGE) {
                tweak_audio(ui, ui.audio.range == v, |p| p.range = v);
            } else if let Some(rest) = id.strip_prefix("__picker_slot_") {
                if let Some((slot, rgb)) = parse_picker_result(rest) {
                    apply_picked_slot_color(ui, slot, rgb);
                }
            } else if let Some(rest) = id.strip_prefix("__picker_audio:") {
                if let Some(rgb) = parse_picker_rgb(rest) {
                    select_audio_color(ui, AudioColor::Solid(rgb));
                }
            } else if let Some(v) = parse_language(&id) {
                select_language(ui, v);
            } else if let Some(slot) = id
                .strip_prefix("slotcolor_")
                .and_then(|s| s.parse::<u8>().ok())
            {
                pick_slot_color(ui, slot);
            } else if id == "audiocolor_rainbow" {
                // muda auto-toggles the check BEFORE the event, so the check
                // already reflects the user's intent: turning the sweep off
                // restores the remembered solid color instead of sticking.
                if ui.audio_rainbow_item.is_checked() {
                    select_audio_color(ui, AudioColor::Rainbow);
                } else {
                    restore_audio_solid(ui);
                }
            } else if id == "pv_audiorainbow_on" {
                // The panel's switch turning the sweep on (its off path has
                // its own id; the menu check above is muda-state and the
                // panel must not depend on it).
                select_audio_color(ui, AudioColor::Rainbow);
            } else if id == "audiocolor_solid_restore" {
                restore_audio_solid(ui);
            } else if id == "audiocolor_solid" {
                pick_audio_color(ui);
            } else if id == "autostart" {
                // muda auto-toggles the check item BEFORE the event arrives,
                // so is_checked() already reflects the user's click.
                let enable = ui.autostart_item.is_checked();
                if settings::autostart_set(enable) {
                    ui.autostart = enable;
                } else {
                    // Registry write failed: undo muda's visual toggle.
                    ui.autostart_item.set_checked(!enable);
                }
            } else if let Some(cmd) = parse_manual_command(&id) {
                // Brightness while a sync runs is software dimming: the
                // engine picks the level up on the next frame (the device
                // stays at max), so the sync keeps running. On the static
                // modes the level goes to the device itself.
                if let UsbCommand::SetBrightness(level) = &cmd {
                    let level = *level;
                    ui.brightness = Some(level);
                    ui.engine.set_brightness(level);
                    if ui.sync == SyncActive::None {
                        ui.engine.send(cmd);
                    }
                } else {
                    // Manual command wins: stop() enqueued SetSession(dead)
                    // on the control channel ahead of this command, and the
                    // USB worker drains the control channel before any
                    // frame, so no sync frame — queued, in flight, or
                    // enqueued late by the dying session — can execute
                    // after it and overwrite it.
                    let was_syncing = ui.sync != SyncActive::None;
                    stop_engine(ui, false);
                    apply_command_state(ui, &cmd);
                    // Mode-setting commands leave the monitor in a normal
                    // mode: they cancel the sync resume memory and a sync
                    // still pending from app start.
                    if matches!(cmd, UsbCommand::SetMode(_) | UsbCommand::SetStaticColor(..)) {
                        ui.resume = None;
                        ui.pending_boot_sync = None;
                    }
                    // An explicit power-off cancels a boot-pending sync too:
                    // auto-starting one on the monitor's return would
                    // override the user's choice.
                    if matches!(cmd, UsbCommand::TurnOff) {
                        ui.pending_boot_sync = None;
                    }
                    // A monitor left armed in sync mode ignores power
                    // commands until its ~12 s sync timeout expires. Sending
                    // Static 1 first makes the power-off effective
                    // immediately. This is a DEVICE-ONLY trick: the active
                    // mode in the UI stays whatever the user had (a later
                    // power-on lights that configuration, not Static 1).
                    if was_syncing && matches!(cmd, UsbCommand::TurnOff) {
                        ui.engine.send(UsbCommand::SetMode(1));
                    }
                    // Turning the LEDs on resumes the remembered sync (image
                    // or audio) if that was the last lighting mode.
                    let mut resumed = false;
                    if matches!(cmd, UsbCommand::TurnOn) {
                        match ui.resume {
                            Some(SyncActive::ImageSync) if !ui.selected_screen.is_empty() => {
                                ui.resume = None;
                                start_sync(ui, SyncActive::ImageSync);
                                resumed = true;
                            }
                            Some(SyncActive::Audio) => {
                                ui.resume = None;
                                start_sync(ui, SyncActive::Audio);
                                resumed = true;
                            }
                            _ => {}
                        }
                        // A sync pending from app start (its screen was not
                        // up yet) counts as the user asking for it back; an
                        // Image Sync still needs its screen on the desktop.
                        // start_sync clears the pending memory.
                        if !resumed {
                            if let Some(which) = ui.pending_boot_sync {
                                if which != SyncActive::ImageSync || !ui.selected_screen.is_empty()
                                {
                                    start_sync(ui, which);
                                    resumed = true;
                                }
                            }
                        }
                    }
                    if !resumed {
                        // Also covers "turn on" while a sync is already
                        // lighting the LEDs (nothing to do) and plain
                        // power-on.
                        if !(matches!(cmd, UsbCommand::TurnOn) && ui.sync != SyncActive::None) {
                            if matches!(cmd, UsbCommand::TurnOn) {
                                // Power-on lights the ACTIVE configuration:
                                // power, then the remembered mode and colors
                                // (the strip may have been reset while off).
                                ui.engine.send(cmd);
                                push_device_state(ui);
                            } else {
                                // With the LEDs off nothing may light them:
                                // mode and color picks stay recorded in the
                                // state and take effect at the next power-on.
                                let send = ui.leds_on || matches!(cmd, UsbCommand::TurnOff);
                                if send {
                                    ui.engine.send(cmd);
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    sync_menu(ui);
    // The panel mirrors whatever this event changed, menu clicks included.
    panel::sync(ui);
    persist(ui, &id);
}

/// Persistence tail of every event: UI and hardware already applied the
/// change by the time this runs; only the disk write is scheduled here.
/// Discrete events write now, rapid-fire ones defer through the debounce
/// timer (see [`save_policy`]), stateless events never write.
fn persist(ui: &Ui, id: &str) {
    match save_policy(id) {
        SavePolicy::Now => settings::save(&current_settings(ui)),
        SavePolicy::Debounced => ui.events.schedule_settings_flush(),
        SavePolicy::Skip => {}
    }
}

/// Which side of the settings-save debounce an event falls on.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum SavePolicy {
    /// Write settings.json now.
    Now,
    /// Restart the debounce; the flush event performs the write when it
    /// fires (~750 ms after the last change).
    Debounced,
    /// No settings state behind this event: never write.
    Skip,
}

/// The save policy per event id. Only the panel's brightness slider emits a
/// burst of events for one user gesture (one per step of a drag, each a
/// different JSON document — the content dedup cannot eat those); every
/// other control is a discrete click.
fn save_policy(id: &str) -> SavePolicy {
    match id {
        // Connect flaps change no settings. The flush event saves in its own
        // arm, so writing again here would only lean on the content dedup.
        usb::CONNECTION_EVENT | events::SETTINGS_FLUSH_EVENT => SavePolicy::Skip,
        _ if id.starts_with("bright_") => SavePolicy::Debounced,
        _ => SavePolicy::Now,
    }
}

/// Final write after the message pump exits: a normal quit already saved in
/// its handler, but a pump exit without the quit event would otherwise drop
/// a debounced save still waiting on its timer. The content dedup makes
/// this free when nothing is pending.
pub fn flush_settings(ui: &Ui) {
    settings::save(&current_settings(ui));
}

/// Sends this app's last-known monitor state through the control channel:
/// brightness, each stored slot color (store-only, no mode switch) and the
/// saved mode. Sync modes 7/8 are excluded — they only make sense behind a
/// running engine, whose init re-arms them. Used at startup and whenever the
/// monitor (re)appears, since commands sent while it was absent were
/// discarded by the USB worker.
fn push_device_state(ui: &Ui) {
    // During sync the device sits at brightness 12 and the engine dims in
    // software — pushing the user's level here would fight the engine.
    if ui.sync == SyncActive::None {
        if let Some(b) = ui.brightness {
            ui.engine.send(UsbCommand::SetBrightness(b));
        }
    }
    for (i, color) in ui.slot_colors.iter().enumerate() {
        if let Some([r, g, b]) = color {
            ui.engine
                .send(UsbCommand::StoreStaticColor(i as u8 + 1, *r, *g, *b));
        }
    }
    if let Some(m) = ui.mode.filter(|m| !matches!(*m, 7 | 8)) {
        ui.engine.send(UsbCommand::SetMode(m));
    }
}

fn current_settings(ui: &Ui) -> Settings {
    Settings {
        sampling: Some(ui.params.sampling),
        smoothing: Some(ui.params.smoothing),
        boost: Some(ui.params.boost),
        fps: Some(ui.params.fps),
        brightness: ui.brightness,
        mode: ui.mode,
        slot_colors: ui.slot_colors,
        image_sync_running: ui.sync == SyncActive::ImageSync,
        audio_running: ui.sync == SyncActive::Audio,
        audio_gain: Some(ui.audio.gain),
        audio_color: Some(ui.audio.color),
        audio_solid: ui.audio_last_solid,
        audio_blink: Some(ui.audio.blink),
        audio_range: Some(ui.audio.range),
        language: Some(ui.language),
    }
}

/// Stops the engine (any mode), optionally restoring Static 1, and resets the
/// menu state.
fn stop_engine(ui: &mut Ui, restore_static1: bool) {
    let was = ui.sync;
    ui.engine.stop();
    if was != SyncActive::None {
        // Remember which sync was lighting the LEDs: "turn LEDs on" will
        // resume it.
        ui.resume = Some(was);
    }
    if restore_static1 {
        // An explicit static restore cancels the resume memory.
        ui.resume = None;
        // Leave the sync mode first, then restore brightness — the disarm
        // sequence every stop path uses. Control sends keep their order off
        // the UI thread without spawning anything. (Hardware-verified:
        // brightness lands even while the monitor stays armed, so
        // mode-first is consistency, not a requirement.)
        let level = ui.brightness.unwrap_or(12);
        let restore_brightness = was != SyncActive::None;
        ui.engine.send(UsbCommand::SetMode(1));
        if restore_brightness {
            ui.engine.send(UsbCommand::SetBrightness(level));
        }
        ui.mode = Some(1);
    } else if was != SyncActive::None {
        // Restore the device brightness for the static modes that follow
        // (during sync the device sat at max and the dimming was
        // software-side, so the monitor is still at 12 here). A single
        // command: nothing behind it to stay ordered against.
        ui.engine
            .send(UsbCommand::SetBrightness(ui.brightness.unwrap_or(12)));
    }
    ui.sync = SyncActive::None;
    refresh_status(ui);
}

fn parse_sampling_mode(id: &str) -> Option<SamplingMode> {
    match id {
        "sampling_5" => Some(SamplingMode::Border5),
        "sampling_15" => Some(SamplingMode::Border15),
        "sampling_full" => Some(SamplingMode::Full),
        _ => None,
    }
}

/// Builds a preset check-item group from a (label, value) table; the item
/// matching `current` starts checked. The id encodes the table index, so
/// localization never changes ids.
fn build_presets<T: Copy + PartialEq>(
    table: &[(Label, T)],
    prefix: &str,
    current: T,
) -> PresetChecks<T> {
    table
        .iter()
        .enumerate()
        .map(|(i, (label, v))| {
            (
                *v,
                CheckMenuItem::with_id(
                    format!("{prefix}_{i}"),
                    label(t()),
                    true,
                    *v == current,
                    None,
                ),
            )
        })
        .collect()
}

/// Applies an Image Sync params tweak and restarts the engine if it was
/// running. `same` must report whether the value was already selected.
fn tweak(ui: &mut Ui, same: bool, set: impl FnOnce(&mut ImageSyncParams)) {
    if same {
        return;
    }
    let prev_sampling = ui.params.sampling;
    set(&mut ui.params);
    // Live-swap: the running loop reads the shared slot every frame.
    ui.engine.set_image_params(ui.params);
    if ui.sync == SyncActive::ImageSync && ui.params.sampling != prev_sampling {
        // A new ring geometry needs new capture regions: only this preset
        // still restarts the session.
        ui.engine.stop();
        start_sync(ui, SyncActive::ImageSync);
    }
}

/// Applies an Audio Sync params tweak live: the running loop re-reads the
/// shared slot every cycle, so nothing restarts (a change here used to cost
/// a ~1 s engine restart with an LED gap).
fn tweak_audio(ui: &mut Ui, same: bool, set: impl FnOnce(&mut AudioParams)) {
    if same {
        return;
    }
    set(&mut ui.audio);
    ui.engine.set_audio_params(ui.audio);
}

/// Applies an audio-sync color change live; the loop rebuilds the base
/// palette from the slot when it sees the new setting. Solid picks are
/// remembered, so a later rainbow-off can restore them.
fn select_audio_color(ui: &mut Ui, v: AudioColor) {
    if ui.audio.color != v {
        if let AudioColor::Solid(rgb) = v {
            ui.audio_last_solid = Some(rgb);
        }
        ui.audio.color = v;
        ui.engine.set_audio_params(ui.audio);
    }
}

/// Switches the rainbow sweep off, back to the last solid color picked
/// (white when none was ever picked). No-op while a solid is showing.
fn restore_audio_solid(ui: &mut Ui) {
    if ui.audio.color == AudioColor::Rainbow {
        let rgb = ui.audio_last_solid.unwrap_or([0xF2, 0xF2, 0xF2]);
        select_audio_color(ui, AudioColor::Solid(rgb));
    }
}

/// Arms `which` sync and starts its engine on the current source/params.
/// Callers that need the old session gone first stop the engine themselves.
/// Any start supersedes a sync still pending from app start.
fn start_sync(ui: &mut Ui, which: SyncActive) {
    ui.pending_boot_sync = None;
    // Arming the monitor lights the strip, whatever it showed before.
    ui.leds_on = true;
    match which {
        SyncActive::ImageSync => {
            ui.sync = SyncActive::ImageSync;
            // The engine init pushes on + video-sync mode (brightness is
            // applied by software dimming at the user's level): mirror it
            // so the checks match what the monitor will actually show.
            ui.mode = Some(VIDEO_SYNC_MODE);
            ui.engine.start(Source::ImageSync {
                screen: ui.selected_screen.clone(),
            });
        }
        SyncActive::Audio => {
            ui.sync = SyncActive::Audio;
            // Audio sync drives the audio-sync mode, absent from the menu:
            // mirror it so the checks match.
            ui.mode = Some(AUDIO_SYNC_MODE);
            ui.engine.start(Source::Audio);
        }
        SyncActive::None => {}
    }
    // The tray surfaces now show the running sync (and clear any stale
    // failure text from a previous self-failed start).
    refresh_status(ui);
}

fn parse_preset<T: Copy>(id: &str, prefix: &str, table: &[(Label, T)]) -> Option<T> {
    let i = id.strip_prefix(prefix)?.parse::<usize>().ok()?;
    table.get(i).map(|&(_, v)| v)
}

/// Switches the UI language live: every `t()` read from now on returns the
/// new table, the tray menu is rebuilt in it and swapped onto the tray, and
/// the tooltip follows. The panel picks the new labels up through its next
/// state snapshot (the common tail after this handler pushes one). The
/// choice persists through the common tail's settings save.
fn select_language(ui: &mut Ui, v: Language) {
    i18n::set_language(v);
    ui.language = v;
    recreate_items(ui);
    if let Some(tray) = ui.tray.as_ref() {
        let menu = build_menu(ui);
        tray.set_menu(Some(Box::new(menu)));
    }
    refresh_status(ui);
}

/// A 16x16 solid-color swatch used as the menu icon of a color entry.
fn swatch_icon([r, g, b]: [u8; 3]) -> Icon {
    let mut rgba = Vec::with_capacity(16 * 16 * 4);
    for _ in 0..16 * 16 {
        rgba.extend_from_slice(&[r, g, b, 255]);
    }
    Icon::from_rgba(rgba, 16, 16).expect("solid swatch")
}

/// Opens the native color picker and applies the picked color to a static
/// slot (stored + activated), exactly like a preset click.
/// Opens the dark picker for a static slot. The pick lands async: the
/// picker reports back a `__picker_slot_N` event once accepted.
fn pick_slot_color(ui: &mut Ui, slot: u8) {
    let initial = ui
        .slot_colors
        .get((slot as usize).saturating_sub(1))
        .copied()
        .flatten()
        .unwrap_or([255, 255, 255]);
    panel::open_picker(&ui.events, PickerRequest::Slot(slot, initial));
}

/// Opens the dark picker for the audio color; on accept the picked color
/// becomes the audio sync solid color (`__picker_audio` event).
fn pick_audio_color(ui: &mut Ui) {
    let initial = match ui.audio.color {
        AudioColor::Solid(rgb) => Some(rgb),
        AudioColor::Rainbow => None,
    };
    panel::open_picker(&ui.events, PickerRequest::Audio(initial));
}

/// Applies an accepted picker result for a static slot: same flow as a
/// preset pick (device disarm, state mirror, resume clearing), deferred
/// until the picker reports the color.
fn apply_picked_slot_color(ui: &mut Ui, slot: u8, rgb: [u8; 3]) {
    let cmd = UsbCommand::SetStaticColor(slot, rgb[0], rgb[1], rgb[2]);
    stop_engine(ui, false);
    apply_command_state(ui, &cmd);
    ui.resume = None;
    // With the LEDs off the pick stays recorded only: it lights at the
    // next power-on, with the rest of the active configuration.
    if ui.leds_on {
        ui.engine.send(cmd);
    }
}

/// Parses "N:R,G,B" from a picker result event.
fn parse_picker_result(rest: &str) -> Option<(u8, [u8; 3])> {
    let (slot, rgb) = rest.split_once(':')?;
    let slot = slot.parse::<u8>().ok()?;
    parse_picker_rgb(rgb).map(|rgb| (slot, rgb))
}

/// Parses "R,G,B".
fn parse_picker_rgb(s: &str) -> Option<[u8; 3]> {
    let mut it = s.split(',');
    let r = it.next()?.parse::<u8>().ok()?;
    let g = it.next()?.parse::<u8>().ok()?;
    let b = it.next()?.parse::<u8>().ok()?;
    Some([r, g, b])
}

fn parse_language(id: &str) -> Option<Language> {
    let i = id.strip_prefix("lang_")?.parse::<usize>().ok()?;
    LANGUAGES.get(i).map(|&(_, l)| l)
}

/// Display name of a device mode 1..=6: translated "Static N"; Peaceful and
/// Dynamic keep LG's own names. Shared by the mode checks, the Mode submenu
/// label and the panel's mode radios.
pub(crate) fn static_mode_name(mode: u8) -> String {
    match mode {
        1..=4 => format!("{} {mode}", t().static_mode),
        5 => "Peaceful".to_string(),
        _ => "Dynamic".to_string(),
    }
}

/// The Mode submenu's label: "Mode - <active>", or plain "Mode" while nothing
/// has been set this session. A running sync wins over the last device mode.
fn mode_label(sync: SyncActive, mode: Option<u8>) -> String {
    let active = match sync {
        SyncActive::ImageSync => "Image Sync".to_string(),
        SyncActive::Audio => "Audio Sync".to_string(),
        SyncActive::None => match mode {
            Some(m @ 1..=6) => static_mode_name(m),
            _ => return t().mode.to_string(),
        },
    };
    format!("{} - {active}", t().mode)
}

/// Re-detects the RGB-strip monitor after a display-topology change. A
/// DeviceName can be renumbered by a re-plug, so a changed detection
/// restarts a running image sync on the new output. The monitor vanishing
/// (standby, input switch) changes nothing immediately: the engine's capture
/// retries idle until the same output returns — and re-arm it (see
/// image_sync_loop) — while a later change event picks up a renumbered one.
///
/// The reappearance itself is also the only signal that the monitor's
/// lighting MCU was reset by the power event: it boots into its factory
/// state (Static 4, blue) with no HID notification, and the USB link usually
/// survives the outage, so [`recover_monitor`] re-asserts the app's state
/// and one delayed pass covers a still-booting MCU.
fn redetect_screen(ui: &mut Ui) {
    let detected = crate::capture::find_lg_output()
        .map(|o| o.name)
        .unwrap_or_default();
    if detected == ui.selected_screen {
        // Same output, but the desktop still changed mode underneath it
        // (HDR toggle, resolution change): the duplication may keep
        // delivering frozen frames until the staleness watchdog catches it
        // (~2 s of frozen LEDs). Drop it so the next frame rebuilds against
        // the new mode; the recreate's retry pacing absorbs the transition.
        if ui.sync == SyncActive::ImageSync {
            ui.engine.invalidate_capturer();
        }
        return;
    }
    ui.selected_screen = detected;
    if ui.selected_screen.is_empty() {
        let text = t().no_screen;
        ui.status_item.set_text(text);
        ui.note = Some(text.to_string());
        return;
    }
    refresh_status(ui);
    match ui.sync {
        SyncActive::ImageSync => {
            // A renumbered DeviceName needs a fresh duplication on the new
            // output; the restart's init also re-arms the reset MCU.
            ui.engine.stop();
            start_sync(ui, SyncActive::ImageSync);
        }
        SyncActive::Audio => {
            // No screen dependency: a controlled reinit of the running audio
            // session re-arms the freshly reset MCU.
            ui.engine.request_reinit();
        }
        SyncActive::None => recover_monitor(ui),
    }
    // The topology usually reports the monitor before its lighting MCU
    // parses reports: one delayed pass re-does whatever the branch above
    // did, from the state current at that point.
    schedule_recovery_repeat(ui);
}

/// Whatever the lighting MCU rebooted into while the monitor was unpowered
/// (factory state: Static 4, blue), re-assert the state this app last set.
/// Runs on the delayed recovery pass and on a topology reappearance with no
/// sync running; reads the CURRENT state, so user changes made after the
/// power event win.
fn recover_monitor(ui: &mut Ui) {
    // With the LEDs off, the strip must come back dark: the freshly booted
    // MCU lit its factory state, so undo it (and leave every deferred
    // change waiting for the next power-on).
    if !ui.leds_on {
        ui.engine.send(UsbCommand::TurnOff);
        return;
    }
    match ui.sync {
        // A controlled engine reinit (frames pause around the re-arm): an
        // out-of-band mode switch landing between two reports of a frame
        // leaves the monitor's parser applying only the first chunk.
        SyncActive::ImageSync => ui.engine.request_reinit(),
        SyncActive::Audio => ui.engine.request_reinit(),
        SyncActive::None => {
            // A sync pending from app start (its screen was not up yet)
            // takes precedence over the static restore, once the monitor's
            // output is really back.
            if ui.pending_boot_sync == Some(SyncActive::ImageSync) && !ui.selected_screen.is_empty()
            {
                start_sync(ui, SyncActive::ImageSync);
            } else {
                push_device_state(ui);
            }
        }
    }
}

/// Schedules one [`RESTORE_PUSH_EVENT`] after [`RECOVERY_REPEAT_DELAY`].
/// Idempotent while a pass is pending: several sources (app start, monitor
/// (re)appearance, resume, topology change) can race within the same second
/// and one pass re-reads the current state anyway. The RESTORE_PUSH handler
/// clears the flag when the pass runs, re-arming the guard.
fn schedule_recovery_repeat(ui: &mut Ui) {
    if ui.recovery_scheduled {
        return;
    }
    ui.recovery_scheduled = true;
    let events = ui.events;
    thread::spawn(move || {
        thread::sleep(RECOVERY_REPEAT_DELAY);
        events.send(RESTORE_PUSH_EVENT.to_string());
    });
}

/// Repaints every check item from Ui state. Runs after each menu event: muda
/// auto-toggles check items on click BEFORE the event arrives, so this
/// restores the visuals to the real state (covers stale toggles, clicking the
/// already-selected entry, and state set by the handlers).
fn sync_menu(ui: &mut Ui) {
    // A sync chosen while the LEDs are off (pending_boot_sync) shows as
    // selected too: it is the active configuration, waiting for power-on.
    ui.toggle_image_sync.set_checked(
        ui.sync == SyncActive::ImageSync || ui.pending_boot_sync == Some(SyncActive::ImageSync),
    );
    ui.toggle_audio.set_checked(
        ui.sync == SyncActive::Audio || ui.pending_boot_sync == Some(SyncActive::Audio),
    );
    // The Mode submenu's label tracks the active lighting mode.
    // A pending (power-deferred) sync names the submenu: it is the active
    // configuration until the strip is powered.
    let shown_sync = if ui.sync == SyncActive::None {
        ui.pending_boot_sync.unwrap_or(ui.sync)
    } else {
        ui.sync
    };
    ui.mode_sub.set_text(mode_label(shown_sync, ui.mode));
    for (level, item) in &ui.bright_items {
        item.set_checked(ui.brightness == Some(*level));
    }
    for (mode, item) in &ui.mode_items {
        item.set_checked(ui.mode == Some(*mode));
    }
    for (mode, item) in &ui.sampling_items {
        item.set_checked(*mode == ui.params.sampling);
    }
    for (v, item) in &ui.smooth_items {
        item.set_checked(ui.params.smoothing == *v);
    }
    for (v, item) in &ui.boost_items {
        item.set_checked(ui.params.boost == *v);
    }
    for (v, item) in &ui.fps_items {
        item.set_checked(ui.params.fps == *v);
    }
    for (v, item) in &ui.gain_items {
        item.set_checked(ui.audio.gain == *v);
    }
    for (v, item) in &ui.audio_blink_items {
        item.set_checked(ui.audio.blink == *v);
    }
    for (v, item) in &ui.audio_range_items {
        item.set_checked(ui.audio.range == *v);
    }
    // Color entries carry the current value in their ICON (a swatch): the
    // picker returns arbitrary colors no fixed check could match.
    for (slot, item) in &ui.slot_color_items {
        let idx = (*slot - 1) as usize;
        if ui.painted_slots[idx] != Some(ui.slot_colors[idx]) {
            item.set_icon(ui.slot_colors[idx].map(swatch_icon));
            ui.painted_slots[idx] = Some(ui.slot_colors[idx]);
        }
    }
    ui.audio_rainbow_item
        .set_checked(ui.audio.color == AudioColor::Rainbow);
    let solid = match ui.audio.color {
        AudioColor::Solid(rgb) => Some(rgb),
        AudioColor::Rainbow => None,
    };
    if ui.painted_audio_solid != Some(solid) {
        ui.audio_solid_item.set_icon(solid.map(swatch_icon));
        ui.painted_audio_solid = Some(solid);
    }
    for (v, item) in &ui.language_items {
        item.set_checked(ui.language == *v);
    }
}

/// Mirrors a manual command's side effects into Ui state so the checks keep
/// matching the monitor. Note set_static_color also switches the device to
/// that slot's mode, so the mode check moves with it.
fn apply_command_state(ui: &mut Ui, cmd: &UsbCommand) {
    match cmd {
        UsbCommand::SetBrightness(level) => ui.brightness = Some(*level),
        UsbCommand::SetMode(mode) => ui.mode = Some(*mode),
        // Pure protocol bookkeeping: no UI state behind it.
        UsbCommand::ArmSync(_) => {}
        UsbCommand::TurnOn => ui.leds_on = true,
        UsbCommand::TurnOff => ui.leds_on = false,
        UsbCommand::SetStaticColor(slot, r, g, b) => {
            let idx = (*slot as usize).saturating_sub(1);
            if let Some(c) = ui.slot_colors.get_mut(idx) {
                *c = Some([*r, *g, *b]);
            }
            ui.mode = Some(*slot);
        }
        UsbCommand::StoreStaticColor(..)
        | UsbCommand::SendColors(..)
        | UsbCommand::SetSession(_)
        | UsbCommand::Probe
        | UsbCommand::Stop
        | UsbCommand::Wake => {}
    }
}

fn parse_manual_command(id: &str) -> Option<UsbCommand> {
    if id == "on" {
        return Some(UsbCommand::TurnOn);
    }
    if id == "off" {
        return Some(UsbCommand::TurnOff);
    }
    if let Some(n) = id.strip_prefix("bright_") {
        return n.parse::<u8>().ok().map(UsbCommand::SetBrightness);
    }
    if let Some(n) = id.strip_prefix("mode_") {
        return n.parse::<u8>().ok().map(UsbCommand::SetMode);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rapid_controls_defer_discrete_actions_save_now() {
        // The brightness slider's burst of events (one per drag step, each a
        // different JSON document): deferred through the debounce timer.
        assert_eq!(save_policy("bright_7"), SavePolicy::Debounced);
        assert_eq!(save_policy("bright_12"), SavePolicy::Debounced);
        // Discrete user actions: written immediately, quit included (that
        // write is the final flush of any pending debounce).
        for id in [
            "mode_3",
            "on",
            "off",
            "toggle_image_sync",
            "toggle_audio",
            "smooth_2",
            "boost_1",
            "fps_3",
            "gain_0",
            "audioblink_1",
            "audiorange_2",
            "sampling_5",
            "lang_4",
            "autostart",
            "pv_autostart",
            "open_panel",
            "slotcolor_3",
            "audiocolor_solid",
            "__picker_slot_2:10,20,30",
            "__picker_audio:1,2,3",
            "quit",
        ] {
            assert_eq!(save_policy(id), SavePolicy::Now, "{id}");
        }
        // Stateless events never write; the flush event saves in its own arm.
        assert_eq!(save_policy(usb::CONNECTION_EVENT), SavePolicy::Skip);
        assert_eq!(save_policy(events::SETTINGS_FLUSH_EVENT), SavePolicy::Skip);
    }
}
