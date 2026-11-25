// vue-native fork: an HDR window's last pass. The scene blends in sRGB-encoded
// space, as it does on an 8-bit surface, into a half-float target whose values
// may exceed 1.0; this linearises it into the extended linear sRGB surface,
// with SDR white at `sdr_white` (SDR white nits / 80).
//
// On a premultiplied surface the value up to SDR white is decoded as it is
// stored, as the compositor decodes an 8-bit surface: un-premultiplying first
// makes a translucent pixel over the desktop several times brighter than in
// SDR. Only the part above SDR white is decoded straight and premultiplied in
// linear light, as in gpui_windows' hdr_output.hlsl.

struct HdrOutput {
    sdr_white: f32,
    premultiplied: u32,
    pad0: u32,
    pad1: u32,
}

@group(0) @binding(0) var<uniform> params: HdrOutput;
@group(0) @binding(1) var hdr_scene: texture_2d<f32>;

@vertex
fn vs_hdr_output(@builtin(vertex_index) id: u32) -> @builtin(position) vec4<f32> {
    let at = vec2<f32>(f32((id << 1u) & 2u), f32(id & 2u));
    return vec4<f32>(at * vec2<f32>(2.0, -2.0) + vec2<f32>(-1.0, 1.0), 0.0, 1.0);
}

fn hdr_decode(encoded: vec3<f32>) -> vec3<f32> {
    let magnitude = abs(encoded);
    return sign(encoded) * select(
        pow((magnitude + 0.055) / 1.055, vec3<f32>(2.4)),
        magnitude / 12.92,
        magnitude < vec3<f32>(0.04045),
    );
}

@fragment
fn fs_hdr_output(@builtin(position) position: vec4<f32>) -> @location(0) vec4<f32> {
    let color = textureLoad(hdr_scene, vec2<i32>(position.xy), 0);
    let alpha = saturate(color.a);
    if (params.premultiplied == 0u) {
        return vec4<f32>(hdr_decode(color.rgb) * params.sdr_white, alpha);
    }
    var straight = vec3<f32>(0.0);
    if (alpha > 0.0) {
        straight = color.rgb / alpha;
    }
    let light = hdr_decode(min(color.rgb, vec3<f32>(alpha)))
        + (hdr_decode(max(straight, vec3<f32>(1.0))) - 1.0) * alpha;
    return vec4<f32>(light * params.sdr_white, alpha);
}
