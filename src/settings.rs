//! Persistent settings: everything that survives a restart. The JSON file
//! lives next to the executable (same convention as the panic log); a missing
//! or corrupt file just means defaults, and the next save rewrites it.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

/// The persisted menu/monitor state. Every field optional: None = unknown
/// (nothing pushed yet). `#[serde(default)]` keeps partial files loadable as
/// the schema grows; unknown keys are ignored.
#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub sampling: Option<crate::sampling::SamplingMode>,
    /// Image Sync temporal smoothing. None = 0.4.
    pub smoothing: Option<f32>,
    /// Image Sync color boost. None = 1.2.
    pub boost: Option<f32>,
    /// Image Sync target FPS. None = 30.
    pub fps: Option<u32>,
    pub brightness: Option<u8>,
    pub mode: Option<u8>,
    /// Colors assigned to each of the monitor's 4 static slots.
    pub slot_colors: [Option<[u8; 3]>; 4],
    /// Whether image sync was running when the app last exited.
    pub image_sync_running: bool,
    /// Whether audio sync was running when the app last exited.
    pub audio_running: bool,
    /// Audio sync loudness → intensity multiplier. None = 1.0 (Media).
    pub audio_gain: Option<f32>,
    /// Audio sync color (rainbow sweep or solid). None = rainbow.
    pub audio_color: Option<crate::audio::AudioColor>,
    /// Audio sync temporal envelope. None = Normal.
    pub audio_blink: Option<crate::audio::Blink>,
    /// Audio sync loudness → level curve. None = Normal.
    pub audio_range: Option<crate::audio::DynamicRange>,
    /// UI language. None = follow the Windows UI language.
    pub language: Option<crate::i18n::Language>,
}

/// settings.json next to the executable. None if the exe path can't be
/// resolved: persistence is best-effort everywhere.
fn settings_path() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;
    Some(dir.join("settings.json"))
}

/// Loads the settings file; any problem (missing, unreadable, corrupt) falls
/// back to defaults instead of failing the app.
pub fn load() -> Settings {
    settings_path()
        .and_then(|path| std::fs::read_to_string(path).ok())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// Bytes of the last successful save, so identical state is not written over
/// and over (every tray click saves).
static LAST_SAVED: OnceLock<Mutex<String>> = OnceLock::new();

/// Writes the settings atomically: a crash mid-write can never leave a
/// truncated settings.json behind. No-op when the serialized state matches
/// the last successful save.
pub fn save(settings: &Settings) {
    let Some(path) = settings_path() else {
        return;
    };
    let Ok(json) = serde_json::to_string_pretty(settings) else {
        return;
    };
    let last = LAST_SAVED.get_or_init(|| Mutex::new(String::new()));
    let Ok(mut last) = last.lock() else {
        return;
    };
    if *last == json {
        return;
    }
    let tmp = path.with_extension("json.tmp");
    if std::fs::write(&tmp, &json).is_ok() && std::fs::rename(&tmp, &path).is_ok() {
        // Only record successes, so a failed write is retried next time.
        *last = json;
    }
}

/// Reads the HKCU Run entry (existence == enabled).
pub fn autostart_enabled() -> bool {
    use windows::core::w;
    use windows::Win32::Foundation::ERROR_SUCCESS;
    use windows::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_SZ};

    // A real buffer (not just a size query) keeps the ERROR_SUCCESS contract
    // uniform across Windows versions.
    let mut buf = [0u16; 512];
    let mut cb = (buf.len() * 2) as u32;
    unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run"),
            w!("lg-ultragear-rgb-control"),
            RRF_RT_REG_SZ,
            None,
            Some(buf.as_mut_ptr().cast()),
            Some(&mut cb),
        ) == ERROR_SUCCESS
    }
}

/// Adds or removes the HKCU Run entry pointing at this executable.
pub fn autostart_set(enable: bool) -> bool {
    use windows::core::w;
    use windows::Win32::Foundation::ERROR_SUCCESS;
    use windows::Win32::System::Registry::{
        RegCloseKey, RegDeleteValueW, RegOpenKeyExW, RegSetValueExW, HKEY, HKEY_CURRENT_USER,
        KEY_SET_VALUE, REG_SZ,
    };

    // Resolve the exe path BEFORE opening the registry key: the early returns
    // below must never skip RegCloseKey (that leaked the HKEY).
    let utf16_path = if enable {
        let exe = match std::env::current_exe() {
            Ok(p) => p,
            Err(_) => return false,
        };
        let Ok(path) = exe.into_os_string().into_string() else {
            return false;
        };
        let mut data = Vec::with_capacity((path.len() + 1) * 2);
        for unit in path.encode_utf16().chain(std::iter::once(0)) {
            data.extend_from_slice(&unit.to_le_bytes());
        }
        data
    } else {
        Vec::new()
    };

    unsafe {
        let mut hkey = HKEY::default();
        if RegOpenKeyExW(
            HKEY_CURRENT_USER,
            w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run"),
            0,
            KEY_SET_VALUE,
            &mut hkey,
        ) != ERROR_SUCCESS
        {
            return false;
        }
        let result = if enable {
            RegSetValueExW(
                hkey,
                w!("lg-ultragear-rgb-control"),
                0,
                REG_SZ,
                Some(&utf16_path),
            )
        } else {
            RegDeleteValueW(hkey, w!("lg-ultragear-rgb-control"))
        };
        let _ = RegCloseKey(hkey);
        result == ERROR_SUCCESS
    }
}
