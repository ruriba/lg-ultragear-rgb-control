// Compute shader that averages each LED's sampling block on the GPU.
// Compiled to cs_5_0 bytecode by build.rs (DXBC is vendor-neutral: every
// driver translates it to its own ISA). Keep this file in sync with the
// CPU-side reference math in src/sampling.rs — the sums are integer and the
// sampling grid comes from the CPU, so both paths are bit-exact.

Texture2D<float4> g_frame : register(t0);
StructuredBuffer<uint4> g_blocks : register(t1); // per LED: [x0,y0,countX,countY] [stride,0,0,0]
cbuffer Cb : register(b0) { uint4 g_fmt; }       // x = HDR flag

RWStructuredBuffer<uint4> g_out : register(u0);  // one uint4 per LED: (sumR,sumG,sumB,count)

groupshared uint gs_r;
groupshared uint gs_g;
groupshared uint gs_b;

// Same mapping as the old CPU f16->sRGB LUT: clamp, linear toe, sRGB curve,
// then trunc(s*255+0.5).
uint srgb_byte(float v) {
    float s;
    if (v <= 0.0) { s = 0.0; }
    else if (v >= 1.0) { s = 1.0; }
    else if (v <= 0.0031308) { s = v * 12.92; }
    else { s = 1.055 * pow(v, 1.0 / 2.4) - 0.055; }
    return (uint)trunc(s * 255.0 + 0.5);
}

[numthreads(256, 1, 1)]
void main(uint3 t : SV_GroupThreadID, uint3 g : SV_GroupID) {
    uint4 geo = g_blocks[g.x * 2u];      // x0, y0, countX, countY
    uint4 par = g_blocks[g.x * 2u + 1u]; // stride
    uint count = geo.z * geo.w;
    uint3 sum = uint3(0, 0, 0);
    for (uint i = t.x; i < count; i += 256u) {
        uint x = geo.x + (i % geo.z) * par.x;
        uint y = geo.y + (i / geo.z) * par.x;
        float4 px = g_frame.Load(int3(x, y, 0));
        uint3 c;
        if (g_fmt.x != 0u) {
            // FP16 scRGB (decoded exactly to float by the SRV).
            c = uint3(srgb_byte(px.r), srgb_byte(px.g), srgb_byte(px.b));
        } else {
            // BGRA8: the UNORM decode is byte/255, so round recovers the
            // exact byte the CPU path would read.
            c = uint3((uint)round(px.r * 255.0), (uint)round(px.g * 255.0),
                      (uint)round(px.b * 255.0));
        }
        sum += c;
    }
    if (t.x == 0u) { gs_r = 0u; gs_g = 0u; gs_b = 0u; }
    GroupMemoryBarrierWithGroupSync();
    InterlockedAdd(gs_r, sum.r);
    InterlockedAdd(gs_g, sum.g);
    InterlockedAdd(gs_b, sum.b);
    GroupMemoryBarrierWithGroupSync();
    if (t.x == 0u) {
        g_out[g.x] = uint4(gs_r, gs_g, gs_b, count);
    }
}
