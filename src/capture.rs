//! Direct DXGI desktop-duplication capture (Windows only).
//!
//! Key property: `frame(timeout)` blocks in the kernel until the desktop
//! produces new content (or the timeout elapses), so a static desktop costs
//! ~0% CPU instead of a polling loop.

use std::io;
use windows::core::Interface;
use windows::Win32::Devices::Display::{
    DisplayConfigGetDeviceInfo, GetDisplayConfigBufferSizes, QueryDisplayConfig,
    DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME, DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME,
    DISPLAYCONFIG_DEVICE_INFO_HEADER, DISPLAYCONFIG_MODE_INFO, DISPLAYCONFIG_PATH_INFO,
    DISPLAYCONFIG_SOURCE_DEVICE_NAME, DISPLAYCONFIG_TARGET_DEVICE_NAME, QDC_ONLY_ACTIVE_PATHS,
};
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN;
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
    D3D11_CREATE_DEVICE_FLAG, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_R16G16B16A16_FLOAT, DXGI_MODE_ROTATION_IDENTITY,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIAdapter1, IDXGIFactory1, IDXGIOutput, IDXGIOutput1,
    IDXGIOutputDuplication, IDXGIResource, DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_INVALID_CALL,
    DXGI_ERROR_MORE_DATA, DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_DESC, DXGI_OUTDUPL_FRAME_INFO,
    DXGI_OUTPUT_DESC,
};

/// A desktop output (monitor) as seen by DXGI.
#[derive(Clone, Debug)]
pub struct OutputInfo {
    /// DeviceName, e.g. `\\.\DISPLAY1`. Stable enough to key screen selection.
    pub name: String,
    /// True when the output's desktop coordinates contain the origin (0,0),
    /// i.e. it is the actual primary monitor.
    pub is_primary: bool,
}

/// Enumerates every desktop-attached output across all adapters, in stable
/// (adapter, output) order.
pub fn enumerate_outputs() -> Vec<OutputInfo> {
    let mut result = Vec::new();
    let Ok(factory) = (unsafe { CreateDXGIFactory1::<IDXGIFactory1>() }) else {
        return result;
    };
    let mut adapter_index = 0u32;
    while let Ok(adapter) = unsafe { factory.EnumAdapters1(adapter_index) } {
        let mut output_index = 0u32;
        while let Ok(output) = unsafe { adapter.EnumOutputs(output_index) } {
            output_index += 1;
            if let Some(info) = output_info(&output) {
                result.push(info);
            }
        }
        adapter_index += 1;
    }
    result
}

fn output_info(output: &IDXGIOutput) -> Option<OutputInfo> {
    let desc = unsafe { output.GetDesc() }.ok()?;
    if !desc.AttachedToDesktop.as_bool() {
        return None;
    }
    let RECT { left, top, .. } = desc.DesktopCoordinates;
    Some(OutputInfo {
        name: wide_str(&desc.DeviceName),
        is_primary: left == 0 && top == 0,
    })
}

/// Monitor models that carry the RGB strip (lowercase; matched against the
/// lowercased EDID name). Mirrors the PIDs in `usb.rs`'s `matches_monitor`
/// (and the list in docs/protocol.md): a new monitor generation needs both
/// updated.
const RGB_MODELS: [&str; 3] = ["27gn950", "38gn950", "38gl950g"];

/// The desktop output of the (first) UltraGear monitor carrying the RGB
/// strip, matched by EDID model — the same trick LG's own software uses (no
/// HID-to-output correlation exists on Windows). Among several, the primary
/// wins. `None` = the strip monitor has no desktop right now (standby,
/// input switch): image sync has no source and must wait.
pub fn find_lg_output() -> Option<OutputInfo> {
    let mut lg: Vec<OutputInfo> = enumerate_outputs()
        .into_iter()
        .filter(|o| monitor_model(&o.name).is_some_and(|m| is_rgb_monitor(&m)))
        .collect();
    lg.sort_by_key(|o| !o.is_primary);
    lg.into_iter().next()
}

/// Is an EDID monitor model one of the RGB-strip ones?
fn is_rgb_monitor(model: &str) -> bool {
    let model = model.to_lowercase();
    RGB_MODELS.iter().any(|k| model.contains(k))
}

/// Resolves the EDID model name of the monitor attached to a DXGI DeviceName
/// (`\\.\DISPLAY1`) via `QueryDisplayConfig`, the documented route to the
/// per-output monitor name. The alternatives don't work for matching:
/// `EnumDisplayDevices`' monitor entry reports "Generic PnP Monitor" for the
/// LG, and its `DeviceID` (the registry EDID route) comes back empty.
fn monitor_model(device_name: &str) -> Option<String> {
    output_models()
        .into_iter()
        .find(|(name, _)| name == device_name)
        .and_then(|(_, model)| model)
}

/// Every active output's DeviceName paired with its monitor's EDID model
/// name, via one `QueryDisplayConfig` pass.
fn output_models() -> Vec<(String, Option<String>)> {
    let mut paths = [DISPLAYCONFIG_PATH_INFO::default(); 64];
    let mut modes = [DISPLAYCONFIG_MODE_INFO::default(); 256];
    let mut path_count = paths.len() as u32;
    let mut mode_count = modes.len() as u32;
    unsafe {
        if GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &mut path_count, &mut mode_count).0
            != 0
            || path_count as usize > paths.len()
            || mode_count as usize > modes.len()
        {
            return Vec::new();
        }
        if QueryDisplayConfig(
            QDC_ONLY_ACTIVE_PATHS,
            &mut path_count,
            paths.as_mut_ptr(),
            &mut mode_count,
            modes.as_mut_ptr(),
            None,
        )
        .0 != 0
        {
            return Vec::new();
        }
        let mut result = Vec::new();
        for path in &paths[..path_count as usize] {
            // The source name carries the GDI DeviceName; the target name
            // carries the EDID monitor name for the same path.
            let mut source = DISPLAYCONFIG_SOURCE_DEVICE_NAME {
                header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
                    r#type: DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME,
                    size: std::mem::size_of::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>() as u32,
                    adapterId: path.sourceInfo.adapterId,
                    id: path.sourceInfo.id,
                },
                ..Default::default()
            };
            if DisplayConfigGetDeviceInfo(&mut source.header) != 0 {
                continue;
            }
            let name = wide_str(&source.viewGdiDeviceName);
            let mut target = DISPLAYCONFIG_TARGET_DEVICE_NAME {
                header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
                    r#type: DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME,
                    size: std::mem::size_of::<DISPLAYCONFIG_TARGET_DEVICE_NAME>() as u32,
                    adapterId: path.targetInfo.adapterId,
                    id: path.targetInfo.id,
                },
                ..Default::default()
            };
            let model = (DisplayConfigGetDeviceInfo(&mut target.header) == 0)
                .then(|| wide_str(&target.monitorFriendlyDeviceName))
                .filter(|m| !m.is_empty());
            result.push((name, model));
        }
        result
    }
}

/// String from a fixed-size NUL-terminated wide field.
fn wide_str(field: &[u16]) -> String {
    let end = field.iter().position(|&c| c == 0).unwrap_or(field.len());
    String::from_utf16_lossy(&field[..end])
}

/// A newly acquired frame: GPU-computed per-block color sums plus the dirty
/// rect verdict. Owned data, so it stays valid until the next `frame()` call.
pub struct FrameData {
    /// Integer (R,G,B) sums over each block's sampling grid; divide by the
    /// matching `counts` entry for the average.
    pub sums: [[u32; 3]; 48],
    pub counts: [u32; 48],
    /// True when this frame's dirty regions overlap the sampled blocks.
    /// Meaningful only for frames whose sampled colors come out unchanged:
    /// overlap = the desktop really changed where we sample (sub-visible
    /// change or frozen duplication); no overlap = the identical colors are
    /// explained by activity elsewhere on the desktop.
    pub dirty_hit: bool,
    /// Per-stage timings when the `LGTRAY_STATS` instrumentation is on.
    pub stats: Option<crate::stats::FrameStats>,
}

#[derive(Debug)]
pub enum CaptureError {
    /// The duplication became invalid (mode change, fullscreen transition,
    /// session switch). The caller must recreate the `Capturer`.
    AccessLost,
    /// The desktop changed mode underneath a duplication that survived it
    /// (an HDR toggle keeps the DeviceName and rarely delivers
    /// WM_DISPLAYCHANGE). The duplication's own descriptor reports the new
    /// mode; the caller must recreate to match it.
    ModeChanged,
    /// This machine cannot run Image Sync at all (GPU without Direct3D 11
    /// compute). Terminal: the session gives up instead of retrying.
    Gpu(String),
    Other(io::Error),
}

impl std::fmt::Display for CaptureError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CaptureError::AccessLost => write!(f, "duplication access lost"),
            CaptureError::ModeChanged => write!(f, "desktop mode changed under the duplication"),
            CaptureError::Gpu(msg) => write!(f, "{msg}"),
            CaptureError::Other(e) => write!(f, "{e}"),
        }
    }
}

fn other_err(context: &str, e: windows::core::Error) -> CaptureError {
    CaptureError::Other(io::Error::other(format!("{context}: {e}")))
}

/// Classifies a failed HRESULT: anything that a duplication recreate can fix
/// becomes `AccessLost`.
fn classify(e: windows::core::Error) -> CaptureError {
    let code = e.code();
    if code == DXGI_ERROR_ACCESS_LOST || code == DXGI_ERROR_INVALID_CALL {
        CaptureError::AccessLost
    } else {
        CaptureError::Other(io::Error::other(format!("DXGI error: {e}")))
    }
}

/// Do any of the frame's dirty rects touch any sampled block? Rects and
/// blocks both use inclusive start / exclusive end coordinates.
fn dirty_overlaps_blocks(dirty: &[RECT], blocks: &[(u16, u16, u16, u16)]) -> bool {
    dirty.iter().any(|d| {
        blocks.iter().any(|&(bx0, by0, bx1, by1)| {
            i32::from(bx0) < d.right
                && i32::from(bx1) > d.left
                && i32::from(by0) < d.bottom
                && i32::from(by1) > d.top
        })
    })
}

pub struct Capturer {
    duplication: IDXGIOutputDuplication,
    /// An acquired duplication frame is pending release.
    frame_held: bool,
    /// Format of the duplication at creation time (log only; the shader
    /// follows the per-frame format, which MPO setups can flip mid-session).
    hdr: bool,
    /// Creation-time desktop mode (format + dimensions). A per-frame
    /// `duplication.GetDesc()` mismatch means the desktop changed mode
    /// under a duplication that survived it.
    creation_format: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT,
    width: usize,
    height: usize,
    blocks: [(u16, u16, u16, u16); 48],
    compute: crate::compute::ComputeAvg,
    /// Reused scratch for the frame's dirty rects (no per-frame allocation).
    dirty_buf: Vec<RECT>,
}

impl Capturer {
    /// Opens a duplication for the output whose DeviceName matches
    /// `output_name`. Strict: if the output doesn't exist (the RGB-strip
    /// monitor's desktop is gone) this returns `Err` and the caller retries,
    /// instead of sampling a different monitor.
    pub fn new(
        output_name: &str,
        sampling: crate::sampling::SamplingMode,
    ) -> Result<Capturer, CaptureError> {
        let (output, adapter, out_desc) = find_output(output_name).ok_or_else(|| {
            CaptureError::Other(io::Error::new(
                io::ErrorKind::NotFound,
                "no desktop output found",
            ))
        })?;

        if out_desc.Rotation != DXGI_MODE_ROTATION_IDENTITY {
            // The duplicated surface is always in physical orientation; we
            // sample ModeDesc dimensions below, so nothing breaks, but the
            // LED-to-screen mapping will look transposed on rotated screens.
            eprintln!(
                "Warning: output {} is rotated; LED mapping may look transposed",
                wide_str(&out_desc.DeviceName)
            );
        }

        // SAFETY: plain COM construction calls; all objects are ref-counted
        // wrappers and only touched from the engine thread.
        unsafe {
            let output1: IDXGIOutput1 = output
                .cast()
                .map_err(|e| other_err("cast IDXGIOutput1", e))?;

            let mut device: Option<ID3D11Device> = None;
            let mut context: Option<ID3D11DeviceContext> = None;
            D3D11CreateDevice(
                &adapter,
                D3D_DRIVER_TYPE_UNKNOWN,
                None,
                D3D11_CREATE_DEVICE_FLAG(0),
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )
            .map_err(|e| other_err("D3D11CreateDevice", e))?;
            let device =
                device.ok_or_else(|| CaptureError::Other(io::Error::other("no D3D11 device")))?;
            let context =
                context.ok_or_else(|| CaptureError::Other(io::Error::other("no D3D11 context")))?;

            let duplication = output1.DuplicateOutput(&device).map_err(classify)?;

            let dup_desc: DXGI_OUTDUPL_DESC = duplication.GetDesc();
            // The duplication descriptor carries the EXACT pixel format and the
            // physical frame dimensions (no guessing from buffer sizes).
            let format = dup_desc.ModeDesc.Format;
            let hdr = match format {
                DXGI_FORMAT_B8G8R8A8_UNORM => false,
                DXGI_FORMAT_R16G16B16A16_FLOAT => true,
                other => {
                    return Err(CaptureError::Other(io::Error::new(
                        io::ErrorKind::Unsupported,
                        format!("unsupported desktop format {:?}", other),
                    )))
                }
            };

            let blocks = crate::sampling::build_sample_blocks(
                dup_desc.ModeDesc.Width as usize,
                dup_desc.ModeDesc.Height as usize,
                sampling,
            );
            // Image Sync runs entirely on GPU compute: no compute shaders
            // means no Image Sync on this machine.
            let compute = crate::compute::ComputeAvg::new(&device, &context, &blocks)
                .map_err(CaptureError::Gpu)?;

            let capturer = Capturer {
                duplication,
                frame_held: false,
                hdr,
                creation_format: format,
                width: dup_desc.ModeDesc.Width as usize,
                height: dup_desc.ModeDesc.Height as usize,
                blocks,
                compute,
                dirty_buf: Vec::new(),
            };
            Ok(capturer)
        }
    }

    /// True when the duplication's creation-time format was HDR FP16 (scRGB);
    /// false for SDR BGRA8. Log only — the shader follows the per-frame
    /// format, which MPO setups can flip mid-session.
    pub fn hdr(&self) -> bool {
        self.hdr
    }

    /// Frame dimensions in pixels (physical orientation).
    pub fn dimensions(&self) -> (usize, usize) {
        (self.width, self.height)
    }

    /// Blocks in `AcquireNextFrame(timeout_ms)` waiting for new desktop
    /// content, then averages the sampled blocks on the GPU.
    ///
    /// While the frame is held (the DXGI contract for metadata queries) its
    /// dirty rects are intersected with the sampled blocks and reported as
    /// `FrameData::dirty_hit`.
    ///
    /// * `Ok(Some(FrameData))` — new frame.
    /// * `Ok(None)` — timeout, desktop unchanged; call again.
    /// * `Err(AccessLost)` — drop this `Capturer` and build a new one.
    pub fn frame(&mut self, timeout_ms: u32) -> Result<Option<FrameData>, CaptureError> {
        let mut stats = crate::stats::enabled().then(crate::stats::FrameStats::default);
        // A desktop mode change with a surviving duplication (an HDR toggle
        // keeps the DeviceName and does not deliver WM_DISPLAYCHANGE for
        // format-only changes) leaves this duplication reporting the new
        // mode while it delivers frozen frames in the stale one — until the
        // staleness watchdog would catch it ~2 s later. The descriptor read
        // is a cheap property getter; one per frame is noise.
        // SAFETY: plain COM property read on the duplication object.
        let dup_desc: DXGI_OUTDUPL_DESC = unsafe { self.duplication.GetDesc() };
        if dup_desc.ModeDesc.Width as usize != self.width
            || dup_desc.ModeDesc.Height as usize != self.height
            || dup_desc.ModeDesc.Format != self.creation_format
        {
            return Err(CaptureError::ModeChanged);
        }
        if self.frame_held {
            // SAFETY: the duplication object outlives the held-frame flag.
            unsafe {
                let _ = self.duplication.ReleaseFrame();
            }
            self.frame_held = false;
        }
        unsafe {
            let t_acquire = crate::stats::Stage::start(stats.is_some());
            let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
            let mut resource: Option<IDXGIResource> = None;
            if let Err(e) = self
                .duplication
                .AcquireNextFrame(timeout_ms, &mut info, &mut resource)
            {
                return if e.code() == DXGI_ERROR_WAIT_TIMEOUT {
                    Ok(None)
                } else {
                    Err(classify(e))
                };
            }
            self.frame_held = true;
            if let Some(s) = stats.as_mut() {
                s.acquire_us = t_acquire.us();
            }
            // Cursor-only metadata frame: the desktop image did not change.
            // Side effect: the pointer itself no longer lights up the LEDs.
            if info.LastPresentTime == 0 {
                let _ = self.duplication.ReleaseFrame();
                self.frame_held = false;
                return Ok(None);
            }
            let resource = resource
                .ok_or_else(|| CaptureError::Other(io::Error::other("frame without resource")))?;
            let t_dirty = crate::stats::Stage::start(stats.is_some());
            // The frame's dirty rects (where the desktop really changed) are
            // only readable while it is held. Conservative default: on any
            // query failure assume the sampled area was touched, so the
            // caller's staleness logic keeps its old behavior.
            self.dirty_buf.clear();
            let mut needed = 0u32;
            let mut dirty_hit = true;
            let rect_size = std::mem::size_of::<RECT>() as u32;
            match self.duplication.GetFrameDirtyRects(
                (self.dirty_buf.capacity() as u32) * rect_size,
                self.dirty_buf.as_mut_ptr(),
                &mut needed,
            ) {
                Ok(()) => {
                    // SAFETY: DXGI wrote exactly needed/rect_size entries.
                    self.dirty_buf.set_len(needed as usize / rect_size as usize);
                    dirty_hit = dirty_overlaps_blocks(&self.dirty_buf, &self.blocks);
                }
                Err(e) if e.code() == DXGI_ERROR_MORE_DATA => {
                    let n = (needed / rect_size) as usize;
                    self.dirty_buf.clear();
                    self.dirty_buf.resize(n, RECT::default());
                    match self.duplication.GetFrameDirtyRects(
                        needed,
                        self.dirty_buf.as_mut_ptr(),
                        &mut needed,
                    ) {
                        Ok(()) => {
                            dirty_hit = dirty_overlaps_blocks(&self.dirty_buf, &self.blocks);
                        }
                        Err(_) => self.dirty_buf.clear(),
                    }
                }
                Err(_) => self.dirty_buf.clear(),
            }
            if let Some(s) = stats.as_mut() {
                s.dirty_us = t_dirty.us();
            }

            let frame_tex: ID3D11Texture2D = resource
                .cast()
                .map_err(|e| other_err("cast ID3D11Texture2D", e))?;
            // Per-frame format sync: on MPO setups the driver can flip the
            // delivered format between FP16 and BGRA8 even while the
            // duplication descriptor keeps reporting the desktop format.
            // The frame texture's own descriptor is the truth, and is passed
            // straight through to the compute path.
            let mut desc = D3D11_TEXTURE2D_DESC::default();
            frame_tex.GetDesc(&mut desc);
            let sums = self
                .compute
                .avg(&frame_tex, desc.Format, stats.as_mut())
                .map_err(|e| CaptureError::Other(io::Error::other(e)))?;
            let counts = self.compute.counts();
            // The readback Map blocked until the dispatch executed, so the
            // frame was read while we still owned it; release it now.
            let _ = self.duplication.ReleaseFrame();
            self.frame_held = false;
            Ok(Some(FrameData {
                sums,
                counts,
                dirty_hit,
                stats,
            }))
        }
    }
}

impl Drop for Capturer {
    fn drop(&mut self) {
        // Best-effort cleanup; the COM wrappers release the objects themselves.
        if self.frame_held {
            unsafe {
                let _ = self.duplication.ReleaseFrame();
            }
        }
    }
}

/// Finds the output with the given DeviceName, returning it together with its
/// adapter and descriptor. Strict: no fallback to another output. When the
/// named monitor's desktop is gone (standby, input switch) capture init
/// fails and the engine's retry loop idles until the topology changes back —
/// image sync only ever samples the RGB-strip monitor itself.
fn find_output(output_name: &str) -> Option<(IDXGIOutput, IDXGIAdapter1, DXGI_OUTPUT_DESC)> {
    let factory = unsafe { CreateDXGIFactory1::<IDXGIFactory1>() }.ok()?;
    let mut adapter_index = 0u32;
    while let Ok(adapter) = unsafe { factory.EnumAdapters1(adapter_index) } {
        let mut output_index = 0u32;
        while let Ok(output) = unsafe { adapter.EnumOutputs(output_index) } {
            output_index += 1;
            let Ok(desc) = (unsafe { output.GetDesc() }) else {
                continue;
            };
            if !desc.AttachedToDesktop.as_bool() {
                continue;
            }
            if wide_str(&desc.DeviceName) == output_name {
                return Some((output, adapter, desc));
            }
        }
        adapter_index += 1;
    }
    None
}
