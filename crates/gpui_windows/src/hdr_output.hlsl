// vue-native fork: an HDR window's last pass. The scene blends in sRGB-encoded
// space, as it does on an 8-bit swap chain, into a half-float target whose
// values may exceed 1.0; this linearises it into the scRGB swap chain, still
// premultiplied, with SDR white at `sdr_white` (SDR white nits / 80).
//
// Up to SDR white the premultiplied value is decoded as it is, as DWM decodes
// an 8-bit swap chain: un-premultiplying first makes a translucent pixel over
// the desktop several times brighter than in SDR. Only the part of the colour
// above SDR white is decoded straight and premultiplied in linear light.

cbuffer HdrOutput : register(b1) {
    float sdr_white;
    float3 hdr_output_pad;
};

Texture2D<float4> hdr_scene : register(t0);

float4 hdr_output_vertex(uint id : SV_VertexID) : SV_Position {
    float2 at = float2((id << 1) & 2, id & 2);
    return float4(at * float2(2.0, -2.0) + float2(-1.0, 1.0), 0.0, 1.0);
}

float3 hdr_decode(float3 encoded) {
    float3 magnitude = abs(encoded);
    return sign(encoded) * lerp(magnitude / 12.92,
        pow((magnitude + 0.055) / 1.055, 2.4), step(0.04045, magnitude));
}

float4 hdr_output_fragment(float4 position : SV_Position) : SV_Target {
    float4 color = hdr_scene.Load(int3(position.xy, 0));
    float alpha = saturate(color.a);
    float3 straight = alpha > 0.0 ? color.rgb / alpha : float3(0.0, 0.0, 0.0);
    float3 light = hdr_decode(min(color.rgb, alpha))
        + (hdr_decode(max(straight, 1.0)) - 1.0) * alpha;
    return float4(light * sdr_white, alpha);
}
