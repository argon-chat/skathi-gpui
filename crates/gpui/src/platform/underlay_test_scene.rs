//! The scenes every renderer's underlay test draws ([`crate::VnUnderlay`]
//! under backdrop readers), and their checks.

use crate::*;
use anyhow::{Context as _, Result, ensure};

/// The window's side, in device pixels.
pub const SIDE: u32 = 64;

/// What reads the backdrop.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reader {
    /// `backdrop-filter`.
    BlurRect,
    /// The built-in `frost`, at strength 0: its backdrop as it is.
    Frost,
}

/// Which underlay sources lie under the page.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sources {
    None,
    /// One tracked for reads, without a picture yet.
    Unpictured,
    /// A red picture in a box rounded by 12 px at (4, 4, 56, 56).
    One,
    /// That one, then a green one at (16, 16, 24, 24) painted after it.
    Two,
}

fn rect(x: f32, y: f32, width: f32, height: f32) -> Bounds<ScaledPixels> {
    Bounds::new(
        point(ScaledPixels(x), ScaledPixels(y)),
        size(ScaledPixels(width), ScaledPixels(height)),
    )
}

fn picture(atlas: &dyn PlatformAtlas, id: usize, bgra: [u8; 4]) -> Result<AtlasTile> {
    let side = size(DevicePixels(4), DevicePixels(4));
    let bytes: Vec<u8> = (0..16).flat_map(|_| bgra).collect();
    atlas
        .get_or_insert_with(
            AtlasKey::Image(RenderImageParams {
                image_id: ImageId(0x5544_0000 + id),
                frame_index: 0,
            }),
            &mut || Ok(Some((side, std::borrow::Cow::Owned(bytes.clone())))),
        )?
        .context("the underlay's tile was not allocated")
}

fn underlay(tile: AtlasTile, bounds: Bounds<ScaledPixels>, radius: f32, key: u64) -> VnUnderlay {
    VnUnderlay {
        order: 0,
        sampling: 0,
        tile,
        bounds,
        clip: rect(0., 0., SIDE as f32, SIDE as f32),
        corner_radii: Corners::all(ScaledPixels(radius)),
        content: bounds,
        opacity: 1.,
        flags: vn_underlay_flags::PICTURE,
        key: [key as u32, 0],
        pad: [0; 2],
    }
}

/// A transparent page with a half-transparent blue band at y >= 44, the
/// sources, then two readers that do not overlap: an unblurred square one at
/// (0, 0, 40, 40), and one blurred by 3 px and rounded by 10 at (42, 14, 22, 40).
pub fn scene(atlas: &dyn PlatformAtlas, reader: Reader, sources: Sources) -> Result<Scene> {
    let mask = ContentMask {
        bounds: rect(0., 0., SIDE as f32, SIDE as f32),
        ..Default::default()
    };
    let mut scene = Scene::default();
    scene.insert_primitive(Quad {
        bounds: rect(0., 44., SIDE as f32, 20.),
        background: Rgba {
            r: 0.,
            g: 0.,
            b: 1.,
            a: 0.5,
        }
        .into(),
        content_mask: mask.clone(),
        ..Default::default()
    });
    let red = picture(atlas, 1, [0, 0, 255, 255])?;
    let green = picture(atlas, 2, [0, 255, 0, 255])?;
    match sources {
        Sources::None => {}
        Sources::Unpictured => scene.vn_insert_underlay(VnUnderlay {
            flags: 0,
            ..underlay(red, rect(4., 4., 56., 56.), 12., 1)
        }),
        Sources::One | Sources::Two => {
            scene.vn_insert_underlay(underlay(red, rect(4., 4., 56., 56.), 12., 1));
            if sources == Sources::Two {
                scene.vn_insert_underlay(underlay(green, rect(16., 16., 24., 24.), 0., 2));
            }
        }
    }
    let readers = [
        (rect(0., 0., 40., 40.), 0., 0.),
        (rect(42., 14., 22., 40.), 10., 3.),
    ];
    for (bounds, radius, blur) in readers {
        match reader {
            Reader::BlurRect => scene.insert_primitive(BackdropBlurRect {
                bounds,
                content_mask: mask.clone(),
                corner_radii: Corners::all(ScaledPixels(radius)),
                blur_radius: ScaledPixels(blur),
                opacity: 1.,
                ..Default::default()
            }),
            Reader::Frost => scene.insert_primitive(EffectQuad {
                effect_id: effect_id::FROST,
                bounds,
                content_mask: mask.clone(),
                corner_radii: Corners::all(ScaledPixels(radius)),
                params: EffectQuad::params_from(&[0., blur, 0., 0., 0., 0., 0., 0.]),
                opacity: 1.,
                backdrop_blur_radius: ScaledPixels(blur),
                scale: 1.,
                flags: EFFECT_FLAG_NEEDS_BACKDROP,
                ..Default::default()
            }),
        }
    }
    scene.finish();
    Ok(scene)
}

fn pixel(image: &image::RgbaImage, x: u32, y: u32) -> [u8; 4] {
    image.get_pixel(x, y).0
}

fn red(p: [u8; 4]) -> bool {
    p[0] > 200 && p[1] < 40 && p[2] < 40 && p[3] > 250
}

/// `image` drew [`Sources::One`] or [`Sources::Two`], `page` the same readers
/// over [`Sources::None`].
pub fn check(image: &image::RgbaImage, page: &image::RgbaImage, sources: Sources) -> Result<()> {
    let same = |x: u32, y: u32, what: &str| -> Result<()> {
        let (got, alone) = (pixel(image, x, y), pixel(page, x, y));
        ensure!(
            got == alone,
            "({x}, {y}) {what}: {got:?}, the page alone is {alone:?}"
        );
        Ok(())
    };
    let at = |x: u32, y: u32| pixel(image, x, y);
    ensure!(
        red(at(10, 20)),
        "the raw backdrop shows the picture: {:?}",
        at(10, 20)
    );
    ensure!(
        red(at(52, 30)),
        "the blurred backdrop shows the picture: {:?}",
        at(52, 30)
    );
    same(5, 5, "in a reader, outside the layer's rounded corner")?;
    same(42, 14, "outside the reader's rounded corner")?;
    same(
        42,
        53,
        "outside the reader's rounded corner, over the translucent band",
    )?;
    same(50, 8, "outside every reader")?;
    if sources == Sources::Two {
        let p = at(28, 28);
        ensure!(
            p[1] > 200 && p[0] < 40 && p[3] > 250,
            "the source painted last is on top: {p:?}"
        );
        ensure!(
            red(at(10, 30)),
            "the first shows beside it: {:?}",
            at(10, 30)
        );
    }
    Ok(())
}
