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

use windows::Win32::Graphics::Direct3D::{
    D3D11_SRV_DIMENSION_BUFFER, D3D11_SRV_DIMENSION_TEXTURE2D, D3D_FEATURE_LEVEL_11_0,
};
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Buffer, ID3D11ComputeShader, ID3D11Device, ID3D11DeviceContext, ID3D11ShaderResourceView,
    ID3D11Texture2D, ID3D11UnorderedAccessView, D3D11_BIND_CONSTANT_BUFFER,
    D3D11_BIND_SHADER_RESOURCE, D3D11_BIND_UNORDERED_ACCESS, D3D11_BUFFER_DESC, D3D11_BUFFER_SRV,
    D3D11_BUFFER_UAV, D3D11_CPU_ACCESS_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ,
    D3D11_RESOURCE_MISC_BUFFER_STRUCTURED, D3D11_SHADER_RESOURCE_VIEW_DESC, D3D11_SUBRESOURCE_DATA,
    D3D11_TEX2D_SRV, D3D11_UAV_DIMENSION_BUFFER, D3D11_USAGE_DEFAULT, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_UNKNOWN;

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

pub struct ComputeAvg {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    shader: ID3D11ComputeShader,
    cbuffer: ID3D11Buffer,
    out_buffer: ID3D11Buffer,
    out_uav: ID3D11UnorderedAccessView,
    blocks_srv: ID3D11ShaderResourceView,
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

            Ok(Self {
                device: device.clone(),
                context: context.clone(),
                shader: cs.ok_or("CreateComputeShader: null")?,
                cbuffer,
                out_buffer,
                out_uav: out_uav.ok_or("out UAV: null")?,
                blocks_srv: blocks_srv.ok_or("blocks SRV: null")?,
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
    pub fn avg(
        &mut self,
        frame_tex: &ID3D11Texture2D,
        hdr: bool,
    ) -> Result<[[u32; 3]; 48], String> {
        unsafe {
            let mut tex_desc = Default::default();
            frame_tex.GetDesc(&mut tex_desc);
            let mut srv = None;
            self.device
                .CreateShaderResourceView(
                    frame_tex,
                    Some(&D3D11_SHADER_RESOURCE_VIEW_DESC {
                        Format: tex_desc.Format,
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
            self.context.Dispatch(LEDS, 1, 1);
            let no_resources: [Option<ID3D11ShaderResourceView>; 2] = [None, None];
            let no_uavs: [Option<ID3D11UnorderedAccessView>; 1] = [None];
            self.context.CSSetShaderResources(0, Some(&no_resources));
            self.context
                .CSSetUnorderedAccessViews(0, 1, Some(no_uavs.as_ptr()), None);

            // Fresh staging buffer per frame — same doctrine as the pixel
            // staging texture (reusing readback buffers returned stale data
            // on real drivers), and at 768 bytes the allocation is noise.
            let mut staging = None;
            self.device
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
            let Some(staging) = staging else {
                return Err("readback buffer: null".into());
            };
            self.context.CopyResource(&staging, &self.out_buffer);
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            self.context
                .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                .map_err(|e| format!("readback Map: {e}"))?;
            // SAFETY: the map returned LEDS * 16 bytes of written results.
            let rows = std::slice::from_raw_parts(mapped.pData as *const [u32; 4], LEDS as usize);
            let mut sums = [[0u32; 3]; 48];
            for (i, row) in rows.iter().enumerate() {
                sums[i] = [row[0], row[1], row[2]];
            }
            self.context.Unmap(&staging, 0);
            Ok(sums)
        }
    }
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
