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
            pad: PolychromeSprite::sampling_word(Some(3.), mode),
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
    near(at(71), (0.5 + 1.5 * 0.345703125) * SDR_WHITE, "cubic HDR edge")?;
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
