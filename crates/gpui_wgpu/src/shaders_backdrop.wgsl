// CSS `backdrop-filter: blur()`: the blurred pyramid composited through a
// rounded rect. Concatenated after the storage-buffer variant of `shaders.wgsl`;
// a port of `blur_rect_*` in gpui_windows' shaders.hlsl.
//
// Group 2 is shared with the `--shading` effects: the processed backdrop at
// binding 0 (`t_sprite`), the clamping sampler at 1 (`s_sprite`) and the
// untouched snapshot at 2, and at 10 the snapshot without `<sk-layer>` pictures.

@group(2) @binding(10) var t_backdrop_page: texture_2d<f32>;

struct BlurRect {
    order: u32,
    pad: u32,
    bounds: Bounds,
    content_mask: Bounds,
    content_mask_radii: Corners,
    content_mask_shapes: CornerShapes,
    corner_radii: Corners,
    blur_radius: f32,
    opacity: f32,
    tint: Hsla,
    corner_shapes: CornerShapes,
}

// 32 words, in `gpui::BackdropBlurRect` order.
fn load_blur_rect(instance_id: u32) -> BlurRect {
    var cursor = instance_cursor(instance_id * 32u);
    return BlurRect(
        read_word(&cursor),
        read_word(&cursor),
        read_bounds(&cursor),
        read_bounds(&cursor),
        read_corners(&cursor),
        read_corner_shapes(&cursor),
        read_corners(&cursor),
        read_f32(&cursor),
        read_f32(&cursor),
        read_hsla(&cursor),
        read_corner_shapes(&cursor),
    );
}

// Keeps rgb <= a, so a premultiplied target never holds an out-of-gamut pixel.
fn clamp_premultiplied(color: vec4<f32>) -> vec4<f32> {
    let alpha = saturate(color.a);
    return vec4<f32>(clamp(color.rgb, vec3<f32>(0.0), vec3<f32>(alpha)), alpha);
}

// `above` arrives with straight alpha, `below` is already premultiplied.
fn over_straight_on_premultiplied(below: vec4<f32>, above: vec4<f32>) -> vec4<f32> {
    let premultiplied = vec4<f32>(above.rgb * above.a, above.a);
    return clamp_premultiplied(premultiplied + below * (1.0 - above.a));
}

// The rect cut to its content mask's rectangle. Nothing here is transformed,
// so drawing the intersection rejects exactly the pixels that DirectX's clip
// distances reject.
fn clip_bounds(bounds: Bounds, mask: Bounds) -> Bounds {
    let origin = max(bounds.origin, mask.origin);
    let corner = min(bounds.origin + bounds.size, mask.origin + mask.size);
    return Bounds(origin, max(corner - origin, vec2<f32>(0.0)));
}

struct BlurRectVarying {
    @builtin(position) position: vec4<f32>,
    @location(0) @interpolate(flat) blur_rect_id: u32,
}

@vertex
fn vs_blur_rect(@builtin(vertex_index) vertex_id: u32, @builtin(instance_index) instance_id: u32) -> BlurRectVarying {
    let unit_vertex = vec2<f32>(f32(vertex_id & 1u), 0.5 * f32(vertex_id & 2u));
    let blur_rect = load_blur_rect(instance_id);
    var out = BlurRectVarying();
    out.position = to_device_position(unit_vertex, clip_bounds(blur_rect.bounds, blur_rect.content_mask));
    out.blur_rect_id = instance_id;
    return out;
}

// Drawn with blending disabled: outside the rounded rect the page alone is
// written back, which keeps the antialiased edge correct.
@fragment
fn fs_blur_rect(input: BlurRectVarying) -> @location(0) vec4<f32> {
    let blur_rect = load_blur_rect(input.blur_rect_id);
    let position = input.position.xy;
    let texture_position = position / globals.viewport_size;

    let sdf = shaped_quad_sdf(position, blur_rect.bounds, blur_rect.corner_radii, blur_rect.corner_shapes);
    let shape_alpha = saturate(0.5 - sdf);

    let original = textureSampleLevel(t_backdrop_page, s_sprite, texture_position, 0.0);
    let blurred = textureSampleLevel(t_sprite, s_sprite, texture_position, 0.0);
    let tint = hsla_to_rgba(blur_rect.tint);
    let tinted = select(blurred, over_straight_on_premultiplied(blurred, tint), tint.a > 0.0);
    let coverage = shape_alpha * saturate(blur_rect.opacity) *
        content_mask_coverage(position, blur_rect.content_mask, blur_rect.content_mask_radii, blur_rect.content_mask_shapes);
    return clamp_premultiplied(mix(original, tinted, vec4<f32>(coverage)));
}

// vue-native fork: an `<sk-layer>` picture (`gpui::VnUnderlay`), drawn
// destination-over into a snapshot copy; the atlas texture is `t_sprite`.
struct VnUnderlay {
    order: u32,
    sampling: u32,
    tile: AtlasTile,
    bounds: Bounds,
    clip: Bounds,
    corner_radii: Corners,
    content: Bounds,
    opacity: f32,
}

// 32 words, in `gpui::VnUnderlay` order; the flags and key are not read.
fn load_vn_underlay(instance_id: u32) -> VnUnderlay {
    var cursor = instance_cursor(instance_id * 32u);
    return VnUnderlay(
        read_word(&cursor),
        read_word(&cursor),
        read_atlas_tile(&cursor),
        read_bounds(&cursor),
        read_bounds(&cursor),
        read_corners(&cursor),
        read_bounds(&cursor),
        read_f32(&cursor),
    );
}

struct VnUnderlayVarying {
    @builtin(position) position: vec4<f32>,
    @location(0) @interpolate(flat) underlay_id: u32,
}

@vertex
fn vs_vn_underlay(@builtin(vertex_index) vertex_id: u32, @builtin(instance_index) instance_id: u32) -> VnUnderlayVarying {
    let unit_vertex = vec2<f32>(f32(vertex_id & 1u), 0.5 * f32(vertex_id & 2u));
    let underlay = load_vn_underlay(instance_id);
    var out = VnUnderlayVarying();
    out.position = to_device_position(unit_vertex, clip_bounds(underlay.bounds, underlay.clip));
    out.underlay_id = instance_id;
    return out;
}

@fragment
fn fs_vn_underlay(input: VnUnderlayVarying) -> @location(0) vec4<f32> {
    let underlay = load_vn_underlay(input.underlay_id);
    let position = input.position.xy;
    let local = (position - underlay.content.origin) / max(underlay.content.size, vec2<f32>(1e-3));
    let atlas_size = vec2<f32>(textureDimensions(t_sprite, 0));
    let tile_clamp = to_tile_clamp(underlay.tile);
    let uv = (vec2<f32>(underlay.tile.bounds.origin) + local * vec2<f32>(underlay.tile.bounds.size)) / atlas_size;
    let color = encode_sampled_texel(
        textureSampleLevel(t_sprite, s_sprite, clamp(uv, tile_clamp.xy, tile_clamp.zw), 0.0),
        underlay.sampling,
    );
    let inset = min(position - underlay.content.origin,
        underlay.content.origin + underlay.content.size - position);
    let coverage = saturate(min(inset.x, inset.y) + 0.5) * underlay.opacity *
        saturate(0.5 - quad_sdf(position, underlay.bounds, underlay.corner_radii));
    let alpha = saturate(color.a) * coverage;
    return vec4<f32>(color.rgb * alpha, alpha);
}
