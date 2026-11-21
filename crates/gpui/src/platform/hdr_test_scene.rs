//! The scene every renderer's HDR output test draws, and its checks.

use crate::*;
use anyhow::{Context as _, Result};

#[allow(missing_docs)]
pub const WIDTH: u32 = 96;
#[allow(missing_docs)]
pub const HEIGHT: u32 = 8;
/// 240-nit SDR white, in scRGB.
pub const SDR_WHITE: f32 = 3.0;

fn rect(x: f32, width: f32) -> Bounds<ScaledPixels> {
    Bounds::new(
        point(ScaledPixels(x), ScaledPixels(0.)),
        size(ScaledPixels(width), ScaledPixels(HEIGHT as f32)),
    )
}

#[allow(missing_docs)]
pub fn f32_to_half(value: f32) -> u16 {
    let bits = value.to_bits();
    let sign = ((bits >> 16) & 0x8000) as u16;
    let exponent = ((bits >> 23) & 0xff) as i32 - 112;
    if value == 0.0 || exponent <= 0 {
        sign
    } else if exponent >= 31 {
        sign | 0x7c00
    } else {
        let truncated = sign | ((exponent as u16) << 10) | ((bits >> 13) & 0x3ff) as u16;
        truncated + ((bits >> 12) & 1) as u16
    }
}

fn encode(linear: f32) -> f32 {
    if linear <= 0.003_130_8 {
        linear * 12.92
    } else {
        1.055 * linear.powf(1.0 / 2.4) - 0.055
    }
}

fn encode_signed(linear: f32) -> f32 {
    encode(linear.abs()).copysign(linear)
}

/// Decode one channel of an extended sRGB render target.
pub fn decode_signed(encoded: f32) -> f32 {
    let magnitude = encoded.abs();
    let linear = if magnitude <= 0.04045 {
        magnitude / 12.92
    } else {
        ((magnitude + 0.055) / 1.055).powf(2.4)
    };
    linear.copysign(encoded)
}

fn eotf(code: u8) -> f32 {
    let c = f32::from(code) / 255.0;
    if c <= 0.04045 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

/// 8-pixel columns on an opaque window: white; #808080; half-transparent
/// blue over black; glyph coverage (gamma-corrected text) in black; a
/// rounded #336699 box, antialiased at its corners; then half-float tiles:
/// 4.0 at headroom 2.5, 4.0 at the default headroom (1.0), and 0.5.
pub fn scene(atlas: &dyn PlatformAtlas) -> Result<Scene> {
    let mask = ContentMask {
        bounds: rect(0., WIDTH as f32),
        ..Default::default()
    };
    let quad = |x: f32, color: Rgba| Quad {
        bounds: rect(x, 8.),
        background: color.into(),
        content_mask: mask.clone(),
        ..Default::default()
    };
    let mut scene = Scene::default();
    scene.insert_primitive(quad(0., rgb(0xffffff)));
    scene.insert_primitive(quad(8., rgb(0x808080)));
    scene.insert_primitive(quad(16., rgb(0x000000)));
    scene.insert_primitive(quad(16., rgba(0x0000ff80)));

    let glyph = size(DevicePixels(8), DevicePixels(HEIGHT as i32));
    let coverage: Vec<u8> = (0..64u32).map(|i| (i * 4) as u8).collect();
    let key = AtlasKey::Svg(RenderSvgParams {
        path: "hdr-output-glyph".into(),
        size: glyph,
    });
    let tile = atlas
        .get_or_insert_with(key, &mut || {
            Ok(Some((glyph, std::borrow::Cow::Owned(coverage.clone()))))
        })?
        .context("the glyph tile was not allocated")?;
    scene.insert_primitive(MonochromeSprite {
        order: 0,
        raw_coverage: false.into(),
        tile,
        bounds: rect(24., 8.),
        content_mask: mask.clone(),
        color: rgb(0x000000).into(),
        transformation: TransformationMatrix::unit(),
        animation: SpriteAnimationId::NONE,
        pad: 0,
    });
    scene.insert_primitive(Quad {
        corner_radii: Corners::all(ScaledPixels(3.5)),
        ..quad(32., rgb(0x336699))
    });

    for (index, (value, headroom)) in [(4.0, 2.5), (4.0, 1.0), (0.5, 2.5)].into_iter().enumerate() {
        let half = f32_to_half(value);
        let one = f32_to_half(1.0);
        let bytes: Vec<u8> = (0..64)
            .flat_map(|_| [half, half, half, one])
            .flat_map(u16::to_le_bytes)
            .collect();
        let key = AtlasKey::Image(RenderImageParams {
            image_id: ImageId(0x4d52_0000 + index),
            frame_index: 0,
        });
        let tile = atlas
            .write_hdr_bytes(&key, glyph, &bytes)?
            .context("the HDR tile was not allocated")?;
        anyhow::ensure!(tile.texture_id.kind == AtlasTextureKind::PolychromeHdr);
        scene.insert_primitive(PolychromeSprite {
            order: 0,
            pad: f32::to_bits(headroom),
            tile,
            grayscale: false.into(),
            opacity: 1.,
            bounds: rect(40. + 8. * index as f32, 8.),
            content_mask: mask.clone(),
            corner_radii: Corners::default(),
            transformation: TransformationMatrix::unit(),
            tint: Hsla::transparent_black(),
            corner_shapes: CornerShapes::default(),
        });
    }
    // The two outer texels belong outside the cropped source rectangle.
    let stripe: Vec<_> = [4.0, 0.5, 0.5, 2.0, 2.0, 4.0]
        .into_iter()
        .flat_map(|v| [v, v, v, 1.])
        .flat_map(|v| f32_to_half(v).to_le_bytes())
        .collect();
    let key = AtlasKey::Image(RenderImageParams {
        image_id: ImageId(0x4d52_0010),
        frame_index: 0,
    });
    let mut tile = atlas
        .write_hdr_bytes(&key, size(DevicePixels(6), DevicePixels(1)), &stripe)?
        .context("the filter test tile was not allocated")?;
    tile.bounds.origin.x.0 += 1;
    tile.bounds.size.width.0 = 4;
    for (x, mode) in [(64., 1), (80., 4)] {
        scene.insert_primitive(PolychromeSprite {
            order: 0,
            pad: PolychromeSprite::sampling_word(Some(3.), mode, false),
            tile: tile.clone(),
            grayscale: false.into(),
            opacity: 1.,
            bounds: rect(x, 16.),
            content_mask: mask.clone(),
            corner_radii: Corners::default(),
            transformation: TransformationMatrix::unit(),
            tint: Hsla::transparent_black(),
            corner_shapes: CornerShapes::default(),
        });
    }
    scene.finish();
    Ok(scene)
}

/// `sdr` is the scene drawn on an 8-bit target, `encoded` the HDR scene
/// target read the same way, `output` the scRGB output.
pub fn check(
    sdr: &image::RgbaImage,
    encoded: &image::RgbaImage,
    output: &[[f32; 4]],
) -> Result<()> {
    for (x, y, expected) in sdr.enumerate_pixels() {
        let actual = encoded.get_pixel(x, y);
        for channel in 0..4 {
            anyhow::ensure!(
                actual.0[channel].abs_diff(expected.0[channel]) <= 1,
                "({x}, {y}): the HDR scene holds {:?} where the 8-bit one holds {:?}",
                actual.0,
                expected.0
            );
        }
        if (40..56).contains(&x) || x >= 64 {
            continue;
        }
        let shown = output[(y * WIDTH + x) as usize];
        for channel in 0..3 {
            let code = encode(shown[channel] / SDR_WHITE) * 255.0;
            anyhow::ensure!(
                (code - f32::from(expected.0[channel])).abs() <= 1.0,
                "({x}, {y}): scRGB {shown:?} is code {code} at SDR white; the 8-bit target shows {:?}",
                expected.0
            );
        }
        anyhow::ensure!(
            (shown[3] - 1.0).abs() < 1e-3,
            "({x}, {y}): alpha {}",
            shown[3]
        );
    }
    let at = |x: u32| output[(4 * WIDTH + x) as usize];
    let near = |actual: [f32; 4], expected: f32, what: &str| -> Result<()> {
        for channel in &actual[..3] {
            anyhow::ensure!(
                (channel - expected).abs() <= expected * 0.005 + 1e-3,
                "{what}: {actual:?}, expected {expected}"
            );
        }
        eprintln!("{what}: scRGB {:.4} (expected {expected:.4})", actual[0]);
        Ok(())
    };
    near(at(4), SDR_WHITE, "SDR white")?;
    near(at(12), eotf(0x80) * SDR_WHITE, "#808080")?;
    near(at(44), 2.5 * SDR_WHITE, "4.0 at headroom 2.5")?;
    near(at(52), SDR_WHITE, "4.0 at headroom 1.0")?;
    near(at(60), 0.5 * SDR_WHITE, "0.5")?;
    near(at(64), 0.5 * SDR_WHITE, "cubic crop edge")?;
    near(at(68), 0.5 * SDR_WHITE, "cubic dark plateau")?;
    near(
        at(71),
        (0.5 + 1.5 * 0.345703125) * SDR_WHITE,
        "cubic HDR edge",
    )?;
    near(at(79), 2.0 * SDR_WHITE, "cubic bright plateau")?;
    near(at(87), 0.5 * SDR_WHITE, "nearest before HDR edge")?;
    near(at(88), 2.0 * SDR_WHITE, "nearest after HDR edge")?;
    Ok(())
}

#[allow(missing_docs)]
pub fn halves_to_pixels(halves: &[u16]) -> Vec<[f32; 4]> {
    halves
        .chunks_exact(4)
        .map(|px| std::array::from_fn(|i| half_to_f32(px[i])))
        .collect()
}

/// Width of the signed surface test scene.
pub const SIGNED_WIDTH: u32 = 128;
const RED: [f32; 3] = [4.15, -0.14, -0.05];
const GREEN: [f32; 3] = [-0.61, 2.83, -0.21];
const NEAR_ZERO: [f32; 3] = [-0.002, 0.002, -0.004];
const NEGATIVE: [f32; 3] = [-0.61, -0.14, -0.05];

/// Draw signed surfaces through sampling, blending, sprite styling and filters.
pub fn signed_scene(atlas: &dyn PlatformAtlas) -> Result<Scene> {
    let content_mask = ContentMask {
        bounds: rect(0., SIGNED_WIDTH as f32),
        ..Default::default()
    };
    let hdr_tile = |index: usize, pixels: &[[f32; 3]]| {
        let bytes: Vec<_> = pixels
            .iter()
            .flat_map(|&[r, g, b]| [r, g, b, 1.])
            .flat_map(|c| f32_to_half(c).to_le_bytes())
            .collect();
        atlas
            .write_hdr_bytes(
                &AtlasKey::Image(RenderImageParams {
                    image_id: ImageId(0x4d52_0100 + index),
                    frame_index: 0,
                }),
                size(DevicePixels(pixels.len() as i32), DevicePixels(1)),
                &bytes,
            )?
            .context("the signed HDR tile was not allocated")
    };
    let red = hdr_tile(0, &[RED])?;
    let green = hdr_tile(1, &[GREEN])?;
    let near_zero = hdr_tile(2, &[NEAR_ZERO])?;
    let negative = hdr_tile(3, &[NEGATIVE])?;
    let mut stripe = hdr_tile(4, &[2., -0.61, -0.61, -0.14, -0.14, 2.].map(|c| [c; 3]))?;
    stripe.bounds.origin.x.0 += 1;
    stripe.bounds.size.width.0 = 4;
    let sprite = |x, width, tile, mode| PolychromeSprite {
        order: 0,
        pad: PolychromeSprite::sampling_word(Some(5.), mode, true),
        tile,
        grayscale: false.into(),
        opacity: 1.,
        bounds: rect(x, width),
        content_mask,
        corner_radii: Corners::default(),
        transformation: TransformationMatrix::unit(),
        tint: Hsla::transparent_black(),
        corner_shapes: CornerShapes::default(),
    };

    let mut scene = Scene::default();
    scene.insert_primitive(sprite(0., 8., red.clone(), 0));
    scene.insert_primitive(sprite(8., 8., green.clone(), 4));
    scene.insert_primitive(sprite(16., 8., near_zero, 0));
    scene.insert_primitive(sprite(24., 8., red.clone(), 0));
    scene.insert_primitive(Quad {
        bounds: rect(24., 8.),
        background: rgba(0x30303080).into(),
        content_mask,
        ..Default::default()
    });
    scene.insert_primitive(PolychromeSprite {
        grayscale: true.into(),
        tint: rgba(0xff000040).into(),
        ..sprite(32., 8., negative, 0)
    });
    scene.insert_primitive(PolychromeSprite {
        pad: PolychromeSprite::sampling_word(Some(5.), 0, false),
        ..sprite(40., 8., red, 0)
    });
    scene.insert_primitive(sprite(48., 16., stripe.clone(), 1));
    scene.insert_primitive(sprite(64., 16., stripe, 4));

    let boundary = HEIGHT as f32;
    for (x, linear_rgb) in [(80., true), (88., false)] {
        let mut values = VnColorMatrix::IDENTITY;
        if linear_rgb {
            values[0] = -1.;
        }
        scene.vn_push_isolation(VnIsolation {
            geometry: std::sync::Arc::new(VnClipGeometry {
                fill_rule: VnFillRule::NonZero,
                bounds: rect(x, 8.),
                segments: vec![
                    [x, 0., x + 8., 0.],
                    [x + 8., 0., x + 8., boundary],
                    [x + 8., boundary, x, boundary],
                    [x, boundary, x, 0.],
                ],
            }),
            color_matrix: Some(VnColorMatrix {
                group_id: x as u64,
                values,
                linear_rgb,
            }),
            text_paint: None,
            content_mask,
            capture: None,
        });
        scene.insert_primitive(sprite(x, 8., green.clone(), 0));
        scene.vn_pop_isolation();
    }
    scene.insert_primitive(PolychromeSprite {
        pad: PolychromeSprite::sampling_word(Some(2.5), 0, true),
        ..sprite(96., 32., green, 0)
    });
    scene.insert_primitive(BackdropBlurRect {
        bounds: rect(104., 16.),
        content_mask,
        blur_radius: ScaledPixels(5.),
        ..Default::default()
    });
    scene.insert_primitive(EffectQuad {
        effect_id: effect_id::FROST,
        bounds: rect(120., 8.),
        content_mask,
        params: EffectQuad::params_from(&[0.5]),
        opacity: 1.,
        scale: 1.,
        flags: EFFECT_FLAG_NEEDS_BACKDROP,
        ..Default::default()
    });
    scene.finish();
    Ok(scene)
}

/// Check output pixels against the signed transfer curve and the 8-bit clamp.
pub fn check_signed(sdr: &image::RgbaImage, output: &[[f32; 4]]) -> Result<()> {
    let y = HEIGHT / 2;
    let check = |x, expected: [f32; 3], name| {
        let actual = output[(y * SIGNED_WIDTH + x) as usize];
        for (actual, expected) in actual.into_iter().zip(expected.map(|c| c * SDR_WHITE)) {
            // The source, encoded intermediate and linear output all round to FP16.
            anyhow::ensure!(
                (actual - expected).abs() <= expected.abs() * 0.0025 + 0.000003,
                "{name}, channel with expected {expected}: {actual}"
            );
        }
        anyhow::ensure!(actual[3] == 1., "{name}: alpha {}", actual[3]);
        eprintln!("{name}: scRGB {actual:.5?}");
        Ok(())
    };
    let half = |c| half_to_f32(f32_to_half(c));
    let red = RED.map(half);
    let green = GREEN.map(half);
    check(4, red, "bilinear signed red")?;
    check(12, green, "nearest signed green")?;
    check(20, NEAR_ZERO.map(half), "transfer curve around zero")?;

    let alpha = 128. / 255.;
    let overlay = 48. / 255.;
    let blend = red.map(|c| decode_signed(encode_signed(c) * (1. - alpha) + overlay * alpha));
    check(28, blend, "translucent overlay on wide-gamut red")?;

    let tint = 64. / 255.;
    let [r, g, b] = NEGATIVE.map(half).map(encode_signed);
    let grayscale = (r * (1. - tint) + tint) * 0.2126 + (g * 0.7152 + b * 0.0722) * (1. - tint);
    check(
        36,
        [decode_signed(grayscale); 3],
        "signed tint and grayscale",
    )?;
    check(44, [red[0], 0., 0.], "legacy HDR word")?;

    let low = half(-0.61);
    let high = half(-0.14);
    check(48, [low; 3], "bounded cubic crop edge")?;
    check(52, [low; 3], "bounded cubic dark plateau")?;
    check(
        55,
        [low + (high - low) * 0.345_703_125; 3],
        "signed cubic edge",
    )?;
    check(63, [high; 3], "bounded cubic bright plateau")?;
    check(71, [low; 3], "nearest before edge")?;
    check(72, [high; 3], "nearest after edge")?;

    check(84, [-green[0], 1., 0.], "linear SVG color matrix")?;
    check(92, [0., 1., 0.], "sRGB SVG color matrix clamp")?;
    check(100, [green[0], 2.5, green[2]], "signed surface headroom")?;
    check(112, [0., 1., 0.], "backdrop blur clamp")?;
    check(124, [0., 1., 0.], "frost clamp")?;

    assert_eq!(sdr.get_pixel(4, y).0, [255, 0, 0, 255]);
    assert_eq!(sdr.get_pixel(12, y).0, [0, 255, 0, 255]);
    Ok(())
}
