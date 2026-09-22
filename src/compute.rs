//! GPU-side block averaging for Image Sync (both sampling modes).
//!
//! Instead of copying the whole desktop (~49 MB/frame at 4K HDR) into a
//! staging texture and reading it back to sample ~3% of the pixels on the
//! CPU, a compute shader walks each LED's sampling grid on the GPU and only
//! the 48 integer color sums (~768 bytes) cross PCIe. The sampling grid
//! (stride, sample counts) is computed on the CPU with the exact integer
//! math of [`crate::sampling::block_stride`], so both paths visit identical
//! pixels. Sums are integers, hence order-independent: the GPU sums were
//! validated bit-for-bit against the CPU pixel walk on real HDR content.
//!
//! The shader is compiled to DXBC at build time (see build.rs): no runtime
//! compiler dependency, and DXBC is vendor-neutral — each GPU driver
//! translates it to its own ISA. Requires Direct3D feature level 11_0 (any
//! GPU of the last ~13 years, including Intel iGPUs and WARP); on anything
//! less, `Capturer::new` fails with `CaptureError::Gpu` and Image Sync is
//! unavailable on that machine.

use crate::stats::{FrameStats, Stage};
use windows::Win32::Graphics::Direct3D::{
    D3D11_SRV_DIMENSION_BUFFER, D3D11_SRV_DIMENSION_TEXTURE2D, D3D_FEATURE_LEVEL_11_0,
};
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Buffer, ID3D11ComputeShader, ID3D11Device, ID3D11DeviceContext, ID3D11Query,
    ID3D11ShaderResourceView, ID3D11Texture2D, ID3D11UnorderedAccessView,
    D3D11_BIND_CONSTANT_BUFFER, D3D11_BIND_SHADER_RESOURCE, D3D11_BIND_UNORDERED_ACCESS,
    D3D11_BUFFER_DESC, D3D11_BUFFER_SRV, D3D11_BUFFER_UAV, D3D11_CPU_ACCESS_READ,
    D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ, D3D11_QUERY, D3D11_QUERY_DATA_TIMESTAMP_DISJOINT,
    D3D11_QUERY_DESC, D3D11_QUERY_TIMESTAMP, D3D11_QUERY_TIMESTAMP_DISJOINT,
    D3D11_RESOURCE_MISC_BUFFER_STRUCTURED, D3D11_SHADER_RESOURCE_VIEW_DESC, D3D11_SUBRESOURCE_DATA,
    D3D11_TEX2D_SRV, D3D11_UAV_DIMENSION_BUFFER, D3D11_USAGE_DEFAULT, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT, DXGI_FORMAT_R16G16B16A16_FLOAT, DXGI_FORMAT_UNKNOWN,
};

/// cs_5_0 bytecode of src/compute.hlsl, compiled by build.rs.
const SHADER_BYTECODE: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/compute.cso"));

const LEDS: u32 = 48;
const ROW_U32: u32 = 16; // sizeof(uint4)

/// Integer sampling parameters per LED, computed with the same math as the
/// CPU sampler so both paths visit identical pixels. Returns the flattened
/// block table uploaded to the shader plus the per-LED sample counts.
pub fn block_dispatch(blocks: &[(u16, u16, u16, u16); 48]) -> (Vec<[u32; 4]>, [u32; 48]) {
    let mut data = Vec::with_capacity((LEDS * 2) as usize);
    let mut counts = [0u32; 48];
    for (i, &(x0, y0, x1, y1)) in blocks.iter().enumerate() {
        let (w, h) = ((x1 - x0) as u32, (y1 - y0) as u32);
        let stride = crate::sampling::block_stride((w * h) as usize) as u32;
        let cx = w.div_ceil(stride);
        let cy = h.div_ceil(stride);
        data.push([x0 as u32, y0 as u32, cx, cy]);
        data.push([stride, 0, 0, 0]);
        counts[i] = cx * cy;
    }
    (data, counts)
}

/// Timestamp-query triple measuring the GPU dispatch duration. Exists only
/// in stats builds (`LGTRAY_STATS` set); lives and dies with the Capturer,
/// so session recreation cleans it up like every other resource.
struct GpuTiming {
    disjoint: ID3D11Query,
    start: ID3D11Query,
    end: ID3D11Query,
}

fn timestamp_queries(device: &ID3D11Device) -> Option<GpuTiming> {
    fn one(device: &ID3D11Device, query: D3D11_QUERY) -> Option<ID3D11Query> {
        let mut q = None;
        unsafe {
            device
                .CreateQuery(
                    &D3D11_QUERY_DESC {
                        Query: query,
                        MiscFlags: 0,
                    },
                    Some(&mut q),
                )
                .ok()?;
        }
        q
    }
    Some(GpuTiming {
        disjoint: one(device, D3D11_QUERY_TIMESTAMP_DISJOINT)?,
        start: one(device, D3D11_QUERY_TIMESTAMP)?,
        end: one(device, D3D11_QUERY_TIMESTAMP)?,
    })
}

pub struct ComputeAvg {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    shader: ID3D11ComputeShader,
    cbuffer: ID3D11Buffer,
    out_buffer: ID3D11Buffer,
    /// CPU-readable result buffer, created once and reused every frame.
    staging: ID3D11Buffer,
    out_uav: ID3D11UnorderedAccessView,
    blocks_srv: ID3D11ShaderResourceView,
    /// Present only when `LGTRAY_STATS` is set.
    gpu_timing: Option<GpuTiming>,
    counts: [u32; 48],
}

impl ComputeAvg {
    /// Builds the compute resources for these blocks. Fails on feature
    /// levels below 11_0 or allocation errors — Image Sync is not available
    /// on such a machine.
    pub fn new(
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        blocks: &[(u16, u16, u16, u16); 48],
    ) -> Result<Self, String> {
        unsafe {
            if device.GetFeatureLevel().0 < D3D_FEATURE_LEVEL_11_0.0 {
                return Err("Direct3D feature level below 11_0 (no compute shaders)".into());
            }
            let mut cs = None;
            device
                .CreateComputeShader(SHADER_BYTECODE, None, Some(&mut cs))
                .map_err(|e| format!("CreateComputeShader: {e}"))?;

            let (block_data, counts) = block_dispatch(blocks);
            let blocks_buffer = default_buffer(
                device,
                (block_data.len() as u32) * ROW_U32,
                D3D11_BIND_SHADER_RESOURCE.0 as u32,
                Some(block_data.as_ptr().cast()),
            )?;
            let out_buffer = default_buffer(
                device,
                LEDS * ROW_U32,
                D3D11_BIND_UNORDERED_ACCESS.0 as u32,
                None,
            )?;
            // Constant buffers must not be structured.
            let mut cbuffer = None;
            device
                .CreateBuffer(
                    &D3D11_BUFFER_DESC {
                        ByteWidth: 16,
                        Usage: D3D11_USAGE_DEFAULT,
                        BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
                        CPUAccessFlags: 0,
                        MiscFlags: 0,
                        StructureByteStride: 0,
                    },
                    None,
                    Some(&mut cbuffer),
                )
                .map_err(|e| format!("cbuffer: {e}"))?;
            let cbuffer = cbuffer.ok_or("cbuffer: null")?;

            let mut blocks_srv = None;
            device
                .CreateShaderResourceView(
                    &blocks_buffer,
                    Some(&D3D11_SHADER_RESOURCE_VIEW_DESC {
                        Format: DXGI_FORMAT_UNKNOWN,
                        ViewDimension: D3D11_SRV_DIMENSION_BUFFER,
                        Anonymous: windows::Win32::Graphics::Direct3D11::D3D11_SHADER_RESOURCE_VIEW_DESC_0 {
                            Buffer: D3D11_BUFFER_SRV {
                                Anonymous1: windows::Win32::Graphics::Direct3D11::D3D11_BUFFER_SRV_0 {
                                    FirstElement: 0,
                                },
                                Anonymous2: windows::Win32::Graphics::Direct3D11::D3D11_BUFFER_SRV_1 {
                                    NumElements: block_data.len() as u32,
                                },
                            },
                        },
                    }),
                    Some(&mut blocks_srv),
                )
                .map_err(|e| format!("blocks SRV: {e}"))?;

            let mut out_uav = None;
            device
                .CreateUnorderedAccessView(
                    &out_buffer,
                    Some(&windows::Win32::Graphics::Direct3D11::D3D11_UNORDERED_ACCESS_VIEW_DESC {
                        Format: DXGI_FORMAT_UNKNOWN,
                        ViewDimension: D3D11_UAV_DIMENSION_BUFFER,
                        Anonymous: windows::Win32::Graphics::Direct3D11::D3D11_UNORDERED_ACCESS_VIEW_DESC_0 {
                            Buffer: D3D11_BUFFER_UAV {
                                FirstElement: 0,
                                NumElements: LEDS,
                                Flags: 0,
                            },
                        },
                    }),
                    Some(&mut out_uav),
                )
                .map_err(|e| format!("out UAV: {e}"))?;

            // One persistent CPU-readable result buffer, reused every frame
            // (previously re-created per frame). The old per-frame doctrine
            // existed because a reuse attempt once returned stale data on
            // real drivers — but stale data is impossible with the ordering
            // below by construction: the CopyResource into this buffer is
            // immediately followed by a blocking Map(D3D11_MAP_READ,
            // flags=0), which cannot return until the GPU has finished
            // writing exactly that copy. (The historical incident predates
            // this ordering analysis; re-validated on hardware in Phase 3 —
            // see docs/report — across idle, full load, MPO staleness, HDR
            // and SDR, and a 45 min soak.)
            let mut staging = None;
            device
                .CreateBuffer(
                    &D3D11_BUFFER_DESC {
                        ByteWidth: LEDS * ROW_U32,
                        Usage: D3D11_USAGE_STAGING,
                        BindFlags: 0,
                        CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                        MiscFlags: D3D11_RESOURCE_MISC_BUFFER_STRUCTURED.0 as u32,
                        StructureByteStride: ROW_U32,
                    },
                    None,
                    Some(&mut staging),
                )
                .map_err(|e| format!("readback buffer: {e}"))?;
            let staging = staging.ok_or("readback buffer: null")?;

            Ok(Self {
                device: device.clone(),
                context: context.clone(),
                shader: cs.ok_or("CreateComputeShader: null")?,
                cbuffer,
                out_buffer,
                staging,
                out_uav: out_uav.ok_or("out UAV: null")?,
                blocks_srv: blocks_srv.ok_or("blocks SRV: null")?,
                gpu_timing: if crate::stats::enabled() {
                    timestamp_queries(device)
                } else {
                    None
                },
                counts,
            })
        }
    }

    /// Per-LED sample counts, for the consumer of [`Self::avg`] sums.
    pub fn counts(&self) -> [u32; 48] {
        self.counts
    }

    /// Averages the acquired frame's blocks on the GPU. `frame_tex` is the
    /// duplication texture, still held by the frame (read-only here).
    /// `format` is the texture's own descriptor format (the per-frame truth
    /// — MPO setups can flip FP16/BGRA8 mid-session); FP16 selects the
    /// scRGB→sRGB conversion. `stats` collects per-stage timings when the
    /// `LGTRAY_STATS` instrumentation is on.
    pub fn avg(
        &mut self,
        frame_tex: &ID3D11Texture2D,
        format: DXGI_FORMAT,
        mut stats: Option<&mut FrameStats>,
    ) -> Result<[[u32; 3]; 48], String> {
        unsafe {
            let hdr = format == DXGI_FORMAT_R16G16B16A16_FLOAT;

            // Timed separately: this is the per-frame resource creation the
            // SRV-cache decision hinges on.
            let t_srv = Stage::start(stats.is_some());
            let mut srv = None;
            self.device
                .CreateShaderResourceView(
                    frame_tex,
                    Some(&D3D11_SHADER_RESOURCE_VIEW_DESC {
                        Format: format,
                        ViewDimension: D3D11_SRV_DIMENSION_TEXTURE2D,
                        Anonymous: windows::Win32::Graphics::Direct3D11::D3D11_SHADER_RESOURCE_VIEW_DESC_0 {
                            Texture2D: D3D11_TEX2D_SRV {
                                MostDetailedMip: 0,
                                MipLevels: 1,
                            },
                        },
                    }),
                    Some(&mut srv),
                )
                .map_err(|e| format!("frame SRV: {e}"))?;
            let Some(srv) = srv else {
                return Err("frame SRV: null".into());
            };
            if let Some(s) = stats.as_deref_mut() {
                s.srv_create_us = t_srv.us();
                s.srv_created += 1;
            }

            let t_submit = Stage::start(stats.is_some());
            let fmt: [u32; 4] = [hdr as u32, 0, 0, 0];
            self.context
                .UpdateSubresource(&self.cbuffer, 0, None, fmt.as_ptr().cast(), 0, 0);

            let resources: [Option<ID3D11ShaderResourceView>; 2] =
                [Some(srv), Some(self.blocks_srv.clone())];
            let uavs: [Option<ID3D11UnorderedAccessView>; 1] = [Some(self.out_uav.clone())];
            self.context.CSSetShader(&self.shader, None);
            self.context.CSSetShaderResources(0, Some(&resources));
            self.context
                .CSSetUnorderedAccessViews(0, 1, Some(uavs.as_ptr()), None);
            if let Some(g) = &self.gpu_timing {
                self.context.Begin(&g.disjoint);
                self.context.End(&g.start);
            }
            self.context.Dispatch(LEDS, 1, 1);
            if let Some(g) = &self.gpu_timing {
                self.context.End(&g.end);
                // Closing the calibration interval is what makes the disjoint
                // query resolvable — without this End, GetData fails and
                // every GPU sample would read NaN.
                self.context.End(&g.disjoint);
            }
            let no_resources: [Option<ID3D11ShaderResourceView>; 2] = [None, None];
            let no_uavs: [Option<ID3D11UnorderedAccessView>; 1] = [None];
            self.context.CSSetShaderResources(0, Some(&no_resources));
            self.context
                .CSSetUnorderedAccessViews(0, 1, Some(no_uavs.as_ptr()), None);
            if let Some(s) = stats.as_deref_mut() {
                s.submit_us = t_submit.us();
            }

            // Reused staging buffer (created in `new`): the copy is followed
            // directly by a blocking Map, which fences the GPU write into
            // this exact buffer — reuse cannot observe stale data. See the
            // creation-site comment for the full argument.
            self.context.CopyResource(&self.staging, &self.out_buffer);
            let t_map = Stage::start(stats.is_some());
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            self.context
                .Map(&self.staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                .map_err(|e| format!("readback Map: {e}"))?;
            if let Some(s) = stats.as_deref_mut() {
                s.map_wait_us = t_map.us();
            }
            // SAFETY: the map returned LEDS * 16 bytes of written results.
            let rows = std::slice::from_raw_parts(mapped.pData as *const [u32; 4], LEDS as usize);
            let mut sums = [[0u32; 3]; 48];
            for (i, row) in rows.iter().enumerate() {
                sums[i] = [row[0], row[1], row[2]];
            }
            self.context.Unmap(&self.staging, 0);
            if let (Some(s), Some(g)) = (stats, self.gpu_timing.as_ref()) {
                s.gpu_ms = read_gpu_ms(&self.context, g);
            }
            Ok(sums)
        }
    }
}

/// Resolves the timestamp pair into a dispatch duration in milliseconds. The
/// blocking readback Map has already fenced the CopyResource queued after
/// the dispatch, so the query data is ready with no extra wait; any failure
/// (not-ready, disjoint interval) yields NaN and just skips the sample.
unsafe fn read_gpu_ms(context: &ID3D11DeviceContext, g: &GpuTiming) -> f32 {
    let mut t_start = 0u64;
    let mut t_end = 0u64;
    let mut dis = D3D11_QUERY_DATA_TIMESTAMP_DISJOINT::default();
    let ok = context
        .GetData(
            &g.start,
            Some(&mut t_start as *mut _ as *mut core::ffi::c_void),
            std::mem::size_of::<u64>() as u32,
            0,
        )
        .is_ok()
        && context
            .GetData(
                &g.end,
                Some(&mut t_end as *mut _ as *mut core::ffi::c_void),
                std::mem::size_of::<u64>() as u32,
                0,
            )
            .is_ok()
        && context
            .GetData(
                &g.disjoint,
                Some(&mut dis as *mut _ as *mut core::ffi::c_void),
                std::mem::size_of_val(&dis) as u32,
                0,
            )
            .is_ok()
        && !dis.Disjoint.as_bool()
        && dis.Frequency > 0;
    if !ok {
        return f32::NAN;
    }
    ((t_end.wrapping_sub(t_start)) as f64 / dis.Frequency as f64 * 1000.0) as f32
}

fn default_buffer(
    device: &ID3D11Device,
    bytes: u32,
    bind: u32,
    init: Option<*const u8>,
) -> Result<ID3D11Buffer, String> {
    let desc = D3D11_BUFFER_DESC {
        ByteWidth: bytes,
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: bind,
        CPUAccessFlags: 0,
        MiscFlags: D3D11_RESOURCE_MISC_BUFFER_STRUCTURED.0 as u32,
        StructureByteStride: ROW_U32,
    };
    let init = init.map(|p| D3D11_SUBRESOURCE_DATA {
        pSysMem: p.cast(),
        SysMemPitch: 0,
        SysMemSlicePitch: 0,
    });
    let mut buf = None;
    unsafe { device.CreateBuffer(&desc, init.as_ref().map(|d| d as *const _), Some(&mut buf)) }
        .map_err(|e| format!("CreateBuffer: {e}"))?;
    buf.ok_or_else(|| "CreateBuffer: null".into())
}
