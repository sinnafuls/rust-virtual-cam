// Letterboxed BGRA → NV12 (BT.601 limited range). Compiled at build time by build.rs.
Texture2D src : register(t0);
SamplerState samp : register(s0);
// Half-texel offsets of the 4-tap box filter for the Y and UV passes.
cbuffer Params : register(b0) { float2 tap_y; float2 tap_uv; };

struct VsOut { float4 pos : SV_Position; float2 uv : TEXCOORD0; };

VsOut vs_main(uint id : SV_VertexID) {
    VsOut o;
    float2 uv = float2((id << 1) & 2, id & 2);
    o.pos = float4(uv * float2(2, -2) + float2(-1, 1), 0, 1);
    o.uv = uv;
    return o;
}

float3 fetch(float2 uv, float2 tap) {
    return 0.25 * (src.Sample(samp, uv + float2(-tap.x, -tap.y)).rgb + src.Sample(samp, uv + float2(tap.x, -tap.y)).rgb
                 + src.Sample(samp, uv + float2(-tap.x,  tap.y)).rgb + src.Sample(samp, uv + float2(tap.x,  tap.y)).rgb);
}

float ps_y(VsOut i) : SV_Target {
    return 0.0627451 + dot(fetch(i.uv, tap_y), float3(0.256788, 0.504129, 0.0979059));
}

float2 ps_uv(VsOut i) : SV_Target {
    float3 c = fetch(i.uv, tap_uv);
    return float2(0.501961 + dot(c, float3(-0.148223, -0.290993, 0.439216)),
                  0.501961 + dot(c, float3(0.439216, -0.367788, -0.0714274)));
}
