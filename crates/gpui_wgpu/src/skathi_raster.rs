//! vue-native fork: `TextRendering::SkathiMode` on Linux, glyph masks rasterized from swash's
//! unhinted outlines by [`gpui::rasterize_dilated`], as on Windows.

use std::{cell::RefCell, rc::Rc};

use anyhow::{Context as _, Result};
use collections::FxHashMap;
use gpui::{
    Bounds, CoverageMask, DevicePixels, FontId, GlyphOutline, HintMetrics, RenderGlyphParams,
    TextRendering, VerticalHint,
};
use swash::{
    FontRef, NormalizedCoord,
    scale::ScaleContext,
    zeno::{Command, PathData},
};

type FaceKey = (FontId, u32, u32);

#[derive(Default)]
struct Cache {
    /// The mask `raster_bounds` built; an atlas miss rasterizes the same params next.
    last: Option<(RenderGlyphParams, CoverageMask)>,
    /// Outlines at phase zero, shared by every phase and luminance level of a glyph.
    outlines: FxHashMap<(FaceKey, u16), Rc<GlyphOutline>>,
    hints: FxHashMap<FontId, Option<HintMetrics>>,
}

thread_local! {
    static CACHE: RefCell<Cache> = RefCell::new(Cache::default());
}

const MAX_OUTLINES: usize = 4096;

pub(crate) fn enabled() -> bool {
    gpui::text_rendering() == TextRendering::SkathiMode
}

/// The luminance level a glyph in `color` is dilated for, plus one; 0 is no dilation.
pub(crate) fn dilation_for_color(color: gpui::Hsla) -> u8 {
    1 + gpui::dilation_level(color.to_rgb())
}

pub(crate) fn raster_bounds(
    font: FontRef,
    coords: &[NormalizedCoord],
    context: &mut ScaleContext,
    params: &RenderGlyphParams,
) -> Result<Bounds<DevicePixels>> {
    let mask = mask(font, coords, context, params)?;
    let bounds = mask.bounds;
    CACHE.with_borrow_mut(|cache| cache.last = Some((params.clone(), mask)));
    Ok(bounds)
}

pub(crate) fn rasterize(
    font: FontRef,
    coords: &[NormalizedCoord],
    context: &mut ScaleContext,
    params: &RenderGlyphParams,
    bounds: Bounds<DevicePixels>,
) -> Result<Vec<u8>> {
    let cached = CACHE.with_borrow_mut(|cache| match cache.last.take() {
        Some((cached, mask)) if cached == *params => Some(mask),
        other => {
            cache.last = other;
            None
        }
    });
    let mask = match cached {
        Some(mask) => mask,
        None => mask(font, coords, context, params)?,
    };
    anyhow::ensure!(
        mask.bounds == bounds,
        "glyph mask bounds {:?} differ from the measured {bounds:?}",
        mask.bounds
    );
    Ok(mask.bytes)
}

fn mask(
    font: FontRef,
    coords: &[NormalizedCoord],
    context: &mut ScaleContext,
    params: &RenderGlyphParams,
) -> Result<CoverageMask> {
    let ppem = params.font_size.as_f32() * params.scale_factor;
    let face_key = (
        params.font_id,
        params.font_size.as_f32().to_bits(),
        params.scale_factor.to_bits(),
    );
    let glyph: u16 = params.glyph_id.0.try_into()?;
    let hinted = params.subpixel_variant.y == 0 && VerticalHint::applies(ppem);
    let (outline, metrics) = CACHE.with_borrow_mut(|cache| -> Result<_> {
        let outline = match cache.outlines.get(&(face_key, glyph)) {
            Some(outline) => outline.clone(),
            None => {
                if cache.outlines.len() >= MAX_OUTLINES {
                    cache.outlines.clear();
                }
                let outline = Rc::new(
                    fetch_outline(font, coords, context, glyph, ppem)
                        .with_context(|| format!("no outline for {params:?}"))?,
                );
                cache.outlines.insert((face_key, glyph), outline.clone());
                outline
            }
        };
        let metrics = if hinted {
            *cache
                .hints
                .entry(params.font_id)
                .or_insert_with(|| hint_metrics(font, coords, context))
        } else {
            None
        };
        Ok((outline, metrics))
    })?;
    let dilation = params
        .dilation
        .checked_sub(1)
        .map_or(0.0, |level| gpui::dilation_amount(ppem, level));
    let hint = metrics.and_then(|metrics| VerticalHint::new(&metrics, ppem, dilation));
    let x_offset = gpui::subpixel_variant_offset(params.subpixel_variant).x;
    Ok(gpui::rasterize_dilated(
        &outline,
        x_offset,
        dilation,
        hint.as_ref(),
    ))
}

/// The glyph's unhinted outline at `size` pixels per em, y down as gpui places glyphs. Taken in
/// font units and scaled here: swash scales TrueType outlines on a 1/64 px grid.
fn fetch_outline(
    font: FontRef,
    coords: &[NormalizedCoord],
    context: &mut ScaleContext,
    glyph: u16,
    size: f32,
) -> Option<GlyphOutline> {
    let unscaled = context
        .builder(font)
        .hint(false)
        .normalized_coords(coords)
        .build()
        .scale_outline(glyph)?;
    let scale = size / font.metrics(&[]).units_per_em as f32;
    let at = |p: swash::zeno::Vector| (p.x * scale, -p.y * scale);
    let mut outline = GlyphOutline::default();
    for command in unscaled.path().commands() {
        match command {
            Command::MoveTo(p) => {
                let (x, y) = at(p);
                outline.move_to(x, y)
            }
            Command::LineTo(p) => {
                let (x, y) = at(p);
                outline.line_to(x, y)
            }
            Command::QuadTo(c, p) => {
                let ((cx, cy), (x, y)) = (at(c), at(p));
                outline.quad_to(cx, cy, x, y)
            }
            Command::CurveTo(c1, c2, p) => {
                let ((x1, y1), (x2, y2), (x, y)) = (at(c1), at(c2), at(p));
                outline.cubic_to(x1, y1, x2, y2, x, y)
            }
            Command::Close => outline.close(),
        }
    }
    outline.close();
    Some(outline)
}

fn hint_metrics(
    font: FontRef,
    coords: &[NormalizedCoord],
    context: &mut ScaleContext,
) -> Option<HintMetrics> {
    let units_per_em = font.metrics(&[]).units_per_em;
    let mut scaler = context
        .builder(font)
        .size(units_per_em as f32)
        .hint(false)
        .normalized_coords(coords)
        .build();
    let mut top = |ch: char| {
        let glyph = font.charmap().map(ch);
        (glyph != 0)
            .then(|| scaler.scale_outline(glyph))
            .flatten()
            .map(|outline| outline.bounds().max.y)
    };
    let (x_height_top, cap_top) = (top('o')?, top('X')?);
    let head = font.table(swash::tag_from_bytes(b"head"))?;
    let box_top = i16::from_be_bytes([*head.get(42)?, *head.get(43)?]);
    Some(HintMetrics {
        units_per_em: units_per_em as f32,
        x_height_top,
        cap_top,
        box_top: box_top as f32,
        stem: 0.0,
    })
}
