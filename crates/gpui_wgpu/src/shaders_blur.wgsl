// The Dual-Kawase backdrop pyramid (`blur_downsample_*` / `blur_upsample_*` in
// gpui_windows' shaders.hlsl), and the copy of an intermediate frame onto a
// surface that cannot be a copy source. Every entry point draws one
// full-target triangle strip with blending disabled.

@group(0) @binding(0) var t_source: texture_2d<f32>;
// Clamps: the taps read outside [0, 1] at the borders, and wrapping would
// bleed the opposite edge in.
@group(0) @binding(1) var s_clamp: sampler;

struct BlurPassVarying {
    @builtin(position) position: vec4<f32>,
    @location(0) texture_position: vec2<f32>,
}

@vertex
fn vs_blur_pass(@builtin(vertex_index) vertex_id: u32) -> BlurPassVarying {
    let unit_vertex = vec2<f32>(f32(vertex_id & 1u), 0.5 * f32(vertex_id & 2u));
    var out = BlurPassVarying();
    out.position = vec4<f32>(unit_vertex * vec2<f32>(2.0, -2.0) + vec2<f32>(-1.0, 1.0), 0.0, 1.0);
    out.texture_position = unit_vertex;
    return out;
}

// A half-float (HDR) target holds light above SDR white as rgb above alpha,
// so its `extended` variants keep rgb only finite and non-negative.
fn clamp_premultiplied(color: vec4<f32>, extended: bool) -> vec4<f32> {
    let alpha = saturate(color.a);
    let bound = select(alpha, 65504.0, extended);
    return vec4<f32>(clamp(color.rgb, vec3<f32>(0.0), vec3<f32>(bound)), alpha);
}

fn source_texel() -> vec2<f32> {
    return 1.0 / vec2<f32>(textureDimensions(t_source, 0));
}

fn blur_downsample(input: BlurPassVarying, extended: bool) -> vec4<f32> {
    let texel = source_texel();
    let uv = input.texture_position;
    var color = textureSample(t_source, s_clamp, uv) * 0.5;
    color += textureSample(t_source, s_clamp, uv + texel * vec2<f32>(-1.0, -1.0)) * 0.125;
    color += textureSample(t_source, s_clamp, uv + texel * vec2<f32>(1.0, -1.0)) * 0.125;
    color += textureSample(t_source, s_clamp, uv + texel * vec2<f32>(-1.0, 1.0)) * 0.125;
    color += textureSample(t_source, s_clamp, uv + texel * vec2<f32>(1.0, 1.0)) * 0.125;
    return clamp_premultiplied(color, extended);
}

fn blur_upsample(input: BlurPassVarying, extended: bool) -> vec4<f32> {
    let texel = source_texel();
    let uv = input.texture_position;
    var color = vec4<f32>(0.0);
    color += textureSample(t_source, s_clamp, uv + texel * vec2<f32>(-1.0, 0.0)) * 0.125;
    color += textureSample(t_source, s_clamp, uv + texel * vec2<f32>(1.0, 0.0)) * 0.125;
    color += textureSample(t_source, s_clamp, uv + texel * vec2<f32>(0.0, -1.0)) * 0.125;
    color += textureSample(t_source, s_clamp, uv + texel * vec2<f32>(0.0, 1.0)) * 0.125;
    color += textureSample(t_source, s_clamp, uv + texel * vec2<f32>(-1.0, -1.0)) * 0.125;
    color += textureSample(t_source, s_clamp, uv + texel * vec2<f32>(1.0, -1.0)) * 0.125;
    color += textureSample(t_source, s_clamp, uv + texel * vec2<f32>(-1.0, 1.0)) * 0.125;
    color += textureSample(t_source, s_clamp, uv + texel * vec2<f32>(1.0, 1.0)) * 0.125;
    return clamp_premultiplied(color, extended);
}

@fragment
fn fs_blur_downsample(input: BlurPassVarying) -> @location(0) vec4<f32> {
    return blur_downsample(input, false);
}

@fragment
fn fs_blur_downsample_extended(input: BlurPassVarying) -> @location(0) vec4<f32> {
    return blur_downsample(input, true);
}

@fragment
fn fs_blur_upsample(input: BlurPassVarying) -> @location(0) vec4<f32> {
    return blur_upsample(input, false);
}

@fragment
fn fs_blur_upsample_extended(input: BlurPassVarying) -> @location(0) vec4<f32> {
    return blur_upsample(input, true);
}

fn srgb_decode_signed(encoded: vec3<f32>) -> vec3<f32> {
    let m = abs(encoded);
    let curve = exp2(2.4 * log2(max((m + 0.055) / 1.055, vec3<f32>(1e-30))));
    return sign(encoded) * select(m / 12.92, curve, m > vec3<f32>(0.04045));
}

// The snapshot as linear light, into a half-float copy of the same size that
// the pyramid of a `BackdropEncoding::Linear` request is built from. Keep in
// sync with `sk_frame_to_linear` in sk.slang.
@fragment
fn fs_backdrop_linear(input: BlurPassVarying) -> @location(0) vec4<f32> {
    let frame = textureLoad(t_source, vec2<i32>(input.position.xy), 0);
    let a = saturate(frame.a);
    let safe = select(1.0, a, a > 0.0);
    let low = srgb_decode_signed(frame.rgb);
    let knee = srgb_decode_signed(vec3<f32>(a)).x;
    let high = knee + (srgb_decode_signed(frame.rgb / safe) - 1.0) * a;
    let light = select(low, select(low, high, frame.rgb > vec3<f32>(a)), a > 0.0);
    return vec4<f32>(light, frame.a);
}

@fragment
fn fs_frame_copy(input: BlurPassVarying) -> @location(0) vec4<f32> {
    return textureLoad(t_source, vec2<i32>(input.position.xy), 0);
}
