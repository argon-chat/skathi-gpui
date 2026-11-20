// vue-native fork: an HDR window's last pass. The scene blends in sRGB-encoded
// space, as it does on an 8-bit surface, into a half-float target whose values
// may exceed 1.0; this linearises it into the extended linear sRGB surface,
// with SDR white at `sdr_white` (SDR white nits / 80).

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

@fragment
fn fs_hdr_output(@builtin(position) position: vec4<f32>) -> @location(0) vec4<f32> {
    let color = textureLoad(hdr_scene, vec2<i32>(position.xy), 0);
    let alpha = saturate(color.a);
    let premultiplied = params.premultiplied != 0u;
    var encoded = color.rgb;
    if (premultiplied) {
        if (alpha > 0.0) {
            encoded /= alpha;
        } else {
            encoded = vec3<f32>(0.0);
        }
    }
    let magnitude = abs(encoded);
    let light = sign(encoded) * select(
        pow((magnitude + 0.055) / 1.055, vec3<f32>(2.4)),
        magnitude / 12.92,
        magnitude < vec3<f32>(0.04045),
    );
    let scale = params.sdr_white * select(1.0, alpha, premultiplied);
    return vec4<f32>(light * scale, alpha);
}
