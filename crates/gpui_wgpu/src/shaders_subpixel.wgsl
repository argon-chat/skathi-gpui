// --- subpixel sprites --- //

struct SubpixelSprite {
    order: u32,
    pad: u32,
    tile: AtlasTile,
    bounds: Bounds,
    content_mask: Bounds,
    // vue-native fork: the content mask's rounded corners; see `content_mask_coverage`.
    content_mask_radii: Corners,
    content_mask_shapes: CornerShapes,
    color: Hsla,
    transformation: TransformationMatrix,
}
// SubpixelSprite has no animation or contrast tail after its transformation.
fn load_subpixel_sprite(instance_id: u32) -> SubpixelSprite {
    var cursor = instance_cursor(instance_id * 36u);
    let order = read_word(&cursor);
    let contrast = read_word(&cursor);
    let tile = read_atlas_tile(&cursor);
    let bounds = read_bounds(&cursor);
    let mask = read_bounds(&cursor);
    let radii = read_corners(&cursor);
    let shapes = read_corner_shapes(&cursor);
    let color = read_hsla(&cursor);
    let transform = read_transformation(&cursor);
    return SubpixelSprite(order, contrast, tile, bounds, mask, radii, shapes, color, transform);
}

struct SubpixelSpriteOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) tile_position: vec2<f32>,
    @location(1) @interpolate(flat) color: vec4<f32>,
    @location(2) @interpolate(flat) tile_clamp: vec4<f32>,
    @location(3) clip_distances: vec4<f32>,
    // vue-native fork: as `MonoSpriteVarying`.
    @location(4) @interpolate(flat) mask_bounds: vec4<f32>,
    @location(5) @interpolate(flat) mask_radii: vec4<f32>,
    @location(7) @interpolate(flat) mask_shapes: vec4<u32>,
    @location(6) @interpolate(flat) contrast: u32,
}

struct SubpixelSpriteFragmentOutput {
    @location(0) @blend_src(0) foreground: vec4<f32>,
    @location(0) @blend_src(1) alpha: vec4<f32>,
}

@vertex
fn vs_subpixel_sprite(@builtin(vertex_index) vertex_id: u32, @builtin(instance_index) instance_id: u32) -> SubpixelSpriteOutput {
    let unit_vertex = vec2<f32>(f32(vertex_id & 1u), 0.5 * f32(vertex_id & 2u));
    let sprite = load_subpixel_sprite(instance_id);

    let gutter = sprite_gutter(sprite.transformation);
    let bounds = grow_sprite_bounds(sprite.bounds, sprite.tile, gutter);
    var out = SubpixelSpriteOutput();
    out.position = to_device_position_transformed(unit_vertex, bounds, sprite.transformation);
    out.tile_position = to_grown_tile_position(unit_vertex, sprite.tile, gutter);
    out.tile_clamp = to_tile_clamp(sprite.tile);
    out.color = hsla_to_rgba(sprite.color);
    out.clip_distances = distance_from_clip_rect_transformed(unit_vertex, bounds, sprite.content_mask, sprite.transformation);
    out.mask_bounds = vec4<f32>(sprite.content_mask.origin, sprite.content_mask.size);
    out.mask_radii = vec4<f32>(sprite.content_mask_radii.top_left, sprite.content_mask_radii.top_right,
                               sprite.content_mask_radii.bottom_right, sprite.content_mask_radii.bottom_left);
    out.mask_shapes = vec4<u32>(sprite.content_mask_shapes.top_left, sprite.content_mask_shapes.top_right,
                                sprite.content_mask_shapes.bottom_right, sprite.content_mask_shapes.bottom_left);
    out.contrast = sprite.pad;
    return out;
}

@fragment
fn fs_subpixel_sprite(input: SubpixelSpriteOutput) -> SubpixelSpriteFragmentOutput {
    let uv = clamp(input.tile_position, input.tile_clamp.xy, input.tile_clamp.zw);
    var sample = textureSample(t_sprite, s_sprite, uv).rgb * tile_gutter_fade(input.tile_position, input.tile_clamp);
    if (gamma_params.is_bgr != 0u) {
        sample = sample.bgr;
    }
    let alpha_corrected = apply_contrast_and_gamma_correction3(sample, input.color.rgb, gamma_params.subpixel_enhanced_contrast, gamma_params.gamma_ratios, input.contrast);

    // Alpha clip after using the derivatives.
    if (any(input.clip_distances < vec4<f32>(0.0))) {
        return SubpixelSpriteFragmentOutput(vec4<f32>(0.0), vec4<f32>(0.0));
    }

    let mask = sprite_mask_coverage(input.position.xy, input.mask_bounds, input.mask_radii, input.mask_shapes);
    var out = SubpixelSpriteFragmentOutput();
    let coverage = input.color.a * alpha_corrected * mask;
    out.foreground = vec4<f32>(input.color.rgb, max(coverage.r, max(coverage.g, coverage.b)));
    out.alpha = vec4<f32>(coverage, 1.0);
    return out;
}
