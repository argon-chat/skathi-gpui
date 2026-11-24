//! vue-native fork: `TextRendering::SkathiMode` on Windows, glyph masks rasterized from
//! DirectWrite's unhinted outlines by [`gpui::rasterize_dilated`].

use std::{cell::RefCell, rc::Rc};

use anyhow::Result;
use collections::FxHashMap;
use gpui::{
    Bounds, CoverageMask, DevicePixels, FontId, GlyphOutline, HintMetrics, RenderGlyphParams,
    TextRendering, VerticalHint,
};
use windows::{
    Win32::Graphics::{Direct2D::Common::*, DirectWrite::*},
    core::*,
};
use windows_numerics::Vector2;

/// A face at one size: what the outline and hint caches are keyed by.
type FaceKey = (FontId, u32, u32);

#[derive(Default)]
struct Cache {
    /// The mask `raster_bounds` built; an atlas miss rasterizes the same params next.
    last: Option<(RenderGlyphParams, CoverageMask)>,
    /// Outlines at phase zero, shared by every phase and luminance level of a glyph.
    outlines: FxHashMap<(FaceKey, u32), Rc<GlyphOutline>>,
    hints: FxHashMap<FaceKey, Option<HintMetrics>>,
    files: FxHashMap<FontId, Option<Rc<FontFile>>>,
    faces: FxHashMap<FaceKey, Option<Rc<ParsedFace>>>,
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
    face: &IDWriteFontFace3,
    params: &RenderGlyphParams,
) -> Result<Bounds<DevicePixels>> {
    let mask = mask(face, params)?;
    let bounds = mask.bounds;
    CACHE.with_borrow_mut(|cache| cache.last = Some((params.clone(), mask)));
    Ok(bounds)
}

pub(crate) fn rasterize(
    face: &IDWriteFontFace3,
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
        None => mask(face, params)?,
    };
    anyhow::ensure!(
        mask.bounds == bounds,
        "glyph mask bounds {:?} differ from the measured {bounds:?}",
        mask.bounds
    );
    Ok(mask.bytes)
}

fn mask(face: &IDWriteFontFace3, params: &RenderGlyphParams) -> Result<CoverageMask> {
    let ppem = params.font_size.as_f32() * params.scale_factor;
    let face_key = (
        params.font_id,
        params.font_size.as_f32().to_bits(),
        params.scale_factor.to_bits(),
    );
    let glyph = params.glyph_id.0 as u16;
    let hinted = params.subpixel_variant.y == 0 && VerticalHint::applies(ppem);
    let (outline, metrics) = CACHE.with_borrow_mut(|cache| -> Result<_> {
        let outline = match cache.outlines.get(&(face_key, glyph as u32)) {
            Some(outline) => outline.clone(),
            None => {
                if cache.outlines.len() >= MAX_OUTLINES {
                    cache.outlines.clear();
                }
                let Cache { files, faces, .. } = cache;
                let parsed = faces.entry(face_key).or_insert_with(|| {
                    let file = files
                        .entry(params.font_id)
                        .or_insert_with(|| FontFile::open(face).map(Rc::new))
                        .clone()?;
                    ParsedFace::new(face, file).map(Rc::new)
                });
                let outline = Rc::new(fetch_outline(face, parsed.as_deref(), glyph, ppem)?);
                cache
                    .outlines
                    .insert((face_key, glyph as u32), outline.clone());
                outline
            }
        };
        let metrics = if hinted {
            *cache
                .hints
                .entry(face_key)
                .or_insert_with(|| hint_metrics(face))
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

/// The glyph's outline from the font's own data, as CoreText and swash read it: DirectWrite's
/// outlines of a TrueType-instructed font come out moved by its vertical hints.
fn fetch_outline(
    face: &IDWriteFontFace3,
    parsed: Option<&ParsedFace>,
    glyph: u16,
    ppem: f32,
) -> Result<GlyphOutline> {
    let Some(ParsedFace { face: parsed, .. }) = parsed else {
        return directwrite_outline(face, glyph, ppem);
    };
    let mut sink = TtfSink {
        outline: GlyphOutline::default(),
        scale: ppem / parsed.units_per_em() as f32,
    };
    parsed.outline_glyph(ttf_parser::GlyphId(glyph), &mut sink);
    sink.outline.close();
    Ok(sink.outline)
}

struct TtfSink {
    outline: GlyphOutline,
    scale: f32,
}

impl ttf_parser::OutlineBuilder for TtfSink {
    fn move_to(&mut self, x: f32, y: f32) {
        self.outline.move_to(x * self.scale, -y * self.scale);
    }

    fn line_to(&mut self, x: f32, y: f32) {
        self.outline.line_to(x * self.scale, -y * self.scale);
    }

    fn quad_to(&mut self, x1: f32, y1: f32, x: f32, y: f32) {
        let s = self.scale;
        self.outline.quad_to(x1 * s, -y1 * s, x * s, -y * s);
    }

    fn curve_to(&mut self, x1: f32, y1: f32, x2: f32, y2: f32, x: f32, y: f32) {
        let s = self.scale;
        self.outline
            .cubic_to(x1 * s, -y1 * s, x2 * s, -y2 * s, x * s, -y * s);
    }

    fn close(&mut self) {
        self.outline.close();
    }
}

/// A face's font file, mapped for as long as the face is cached.
struct FontFile {
    stream: IDWriteFontFileStream,
    context: *mut core::ffi::c_void,
    data: *const u8,
    len: usize,
    index: u32,
}

impl FontFile {
    fn open(face: &IDWriteFontFace3) -> Option<Self> {
        unsafe {
            let mut count = 1;
            let mut files = [None];
            face.GetFiles(&mut count, Some(files.as_mut_ptr())).ok()?;
            let file: IDWriteFontFile = files[0].take()?;
            let (mut key, mut key_len) = (std::ptr::null_mut(), 0);
            file.GetReferenceKey(&mut key, &mut key_len).ok()?;
            let stream = file
                .GetLoader()
                .ok()?
                .CreateStreamFromKey(key, key_len)
                .ok()?;
            let len = stream.GetFileSize().ok()?;
            let (mut data, mut context) = (std::ptr::null_mut(), std::ptr::null_mut());
            stream
                .ReadFileFragment(&mut data, 0, len, &mut context)
                .ok()?;
            Some(Self {
                stream,
                context,
                data: data as *const u8,
                len: len as usize,
                index: face.GetIndex(),
            })
        }
    }

    fn bytes(&self) -> &[u8] {
        // SAFETY: the fragment stays valid until `ReleaseFileFragment` in `drop`.
        unsafe { std::slice::from_raw_parts(self.data, self.len) }
    }
}

/// A face's font data parsed at the face's axis values, for a face DirectWrite does not simulate.
struct ParsedFace {
    // Declared first: dropped before the file it borrows.
    face: ttf_parser::Face<'static>,
    _file: Rc<FontFile>,
}

impl ParsedFace {
    fn new(face: &IDWriteFontFace3, file: Rc<FontFile>) -> Option<Self> {
        if unsafe { face.GetSimulations() } != DWRITE_FONT_SIMULATIONS_NONE {
            return None;
        }
        // SAFETY: `_file` keeps the bytes mapped for as long as the parsed face lives.
        let bytes: &'static [u8] = unsafe { std::mem::transmute(file.bytes()) };
        let mut parsed = ttf_parser::Face::parse(bytes, file.index).ok()?;
        if let Ok(face) = face.cast::<IDWriteFontFace5>() {
            let count = unsafe { face.GetFontAxisValueCount() } as usize;
            let mut axes = vec![DWRITE_FONT_AXIS_VALUE::default(); count];
            if count > 0 && unsafe { face.GetFontAxisValues(&mut axes) }.is_ok() {
                for axis in axes {
                    let tag = ttf_parser::Tag::from_bytes(&axis.axisTag.0.to_le_bytes());
                    parsed.set_variation(tag, axis.value);
                }
            }
        }
        Some(Self {
            face: parsed,
            _file: file,
        })
    }
}

impl Drop for FontFile {
    fn drop(&mut self) {
        unsafe { self.stream.ReleaseFileFragment(self.context) };
    }
}

fn directwrite_outline(face: &IDWriteFontFace3, glyph: u16, ppem: f32) -> Result<GlyphOutline> {
    let outline = Rc::new(RefCell::new(GlyphOutline::default()));
    let sink: ID2D1SimplifiedGeometrySink = OutlineSink {
        outline: outline.clone(),
    }
    .into();
    unsafe { face.GetGlyphRunOutline(ppem, &glyph, None, None, 1, false, false, &sink)? };
    drop(sink);
    Ok(outline.take())
}

fn hint_metrics(face: &IDWriteFontFace3) -> Option<HintMetrics> {
    let mut font = DWRITE_FONT_METRICS1::default();
    let codepoints = ['o' as u32, 'X' as u32];
    let mut glyphs = [0u16; 2];
    let mut metrics = [DWRITE_GLYPH_METRICS::default(); 2];
    unsafe {
        face.GetMetrics(&mut font);
        face.GetGlyphIndices(codepoints.as_ptr(), 2, glyphs.as_mut_ptr())
            .ok()?;
        if glyphs.contains(&0) {
            return None;
        }
        face.GetDesignGlyphMetrics(glyphs.as_ptr(), 2, metrics.as_mut_ptr(), false)
            .ok()?;
    }
    let top = |m: &DWRITE_GLYPH_METRICS| m.verticalOriginY as f32 - m.topSideBearing as f32;
    Some(HintMetrics {
        units_per_em: font.Base.designUnitsPerEm as f32,
        x_height_top: top(&metrics[0]),
        cap_top: top(&metrics[1]),
        box_top: font.glyphBoxTop as f32,
        stem: 0.0,
    })
}

#[implement(ID2D1SimplifiedGeometrySink)]
struct OutlineSink {
    outline: Rc<RefCell<GlyphOutline>>,
}

#[allow(non_snake_case)]
impl ID2D1SimplifiedGeometrySink_Impl for OutlineSink_Impl {
    fn SetFillMode(&self, _fill_mode: D2D1_FILL_MODE) {}

    fn SetSegmentFlags(&self, _flags: D2D1_PATH_SEGMENT) {}

    fn BeginFigure(&self, start: &Vector2, _begin: D2D1_FIGURE_BEGIN) {
        self.outline.borrow_mut().move_to(start.X, start.Y);
    }

    fn AddLines(&self, points: *const Vector2, count: u32) {
        if points.is_null() || count == 0 {
            return;
        }
        // SAFETY: DirectWrite passes `count` points that live for the call.
        let points = unsafe { std::slice::from_raw_parts(points, count as usize) };
        let mut outline = self.outline.borrow_mut();
        for p in points {
            outline.line_to(p.X, p.Y);
        }
    }

    fn AddBeziers(&self, beziers: *const D2D1_BEZIER_SEGMENT, count: u32) {
        if beziers.is_null() || count == 0 {
            return;
        }
        // SAFETY: DirectWrite passes `count` segments that live for the call.
        let beziers = unsafe { std::slice::from_raw_parts(beziers, count as usize) };
        let mut outline = self.outline.borrow_mut();
        for b in beziers {
            outline.cubic_to(
                b.point1.X, b.point1.Y, b.point2.X, b.point2.Y, b.point3.X, b.point3.Y,
            );
        }
    }

    fn EndFigure(&self, _end: D2D1_FIGURE_END) {
        self.outline.borrow_mut().close();
    }

    fn Close(&self) -> windows::core::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::DirectWriteTextSystem;
    use gpui::{PlatformTextSystem, RenderGlyphParams, point, px};
    use std::borrow::Cow;

    /// A glyph's mask as `(x, y, byte)` for its nonzero pixels, relative to the glyph origin.
    fn placed(
        text_system: &dyn PlatformTextSystem,
        params: &RenderGlyphParams,
    ) -> anyhow::Result<Vec<(i32, i32, u8)>> {
        let bounds = text_system.glyph_raster_bounds(params)?;
        if bounds.size.width.0 == 0 {
            return Ok(Vec::new());
        }
        let (size, bytes) = text_system.rasterize_glyph(params, bounds)?;
        let width = size.width.0;
        Ok(bytes
            .iter()
            .enumerate()
            .filter(|(_, byte)| **byte != 0)
            .map(|(i, byte)| {
                let (column, row) = (i as i32 % width, i as i32 / width);
                (bounds.origin.x.0 + column, bounds.origin.y.0 + row, *byte)
            })
            .collect())
    }

    /// Linux draws skathi-mode glyphs from swash's outlines with the same rasterizer: a bundled
    /// font must come out as it does on Windows.
    #[test]
    fn linux_masks_equal_windows_masks() -> anyhow::Result<()> {
        if !super::enabled() {
            return Ok(());
        }
        let bytes = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../assets/fonts/ibm-plex-sans/IBMPlexSans-Regular.ttf"
        ))?;
        let windows = DirectWriteTextSystem::new()?;
        windows.add_fonts(vec![Cow::Owned(bytes.clone())])?;
        let linux = gpui_wgpu::CosmicTextSystem::new_without_system_fonts("IBM Plex Sans");
        linux.add_fonts(vec![Cow::Owned(bytes)])?;
        let font = gpui::font("IBM Plex Sans");
        let (on_windows, on_linux) = (windows.font_id(&font)?, linux.font_id(&font)?);
        let mut worst = 0;
        let mut masks = 0;
        for ch in "Hamburgefonstiv0123&@Q".chars() {
            let glyph = windows.glyph_for_char(on_windows, ch).unwrap();
            assert_eq!(Some(glyph), linux.glyph_for_char(on_linux, ch));
            for (size, scale) in [
                (11.0, 1.0),
                (13.0, 1.0),
                (13.0, 1.25),
                (16.0, 1.5),
                (13.0, 2.0),
            ] {
                for phase in [0, 20, 30] {
                    for dilation in [0, 1, 5] {
                        let params = |font_id| RenderGlyphParams {
                            font_id,
                            glyph_id: glyph,
                            font_size: px(size),
                            subpixel_variant: point(phase, 0),
                            scale_factor: scale,
                            is_emoji: false,
                            subpixel_rendering: false,
                            dilation,
                        };
                        let (w, l) = (params(on_windows), params(on_linux));
                        let a = placed(&windows, &w)?;
                        let b = placed(&linux, &l)?;
                        // Compared over both masks: a bound may differ by a row whose bytes
                        // round to a code or two.
                        let at = |mask: &[(i32, i32, u8)], x, y| {
                            mask.iter()
                                .find(|p| (p.0, p.1) == (x, y))
                                .map_or(0, |p| p.2)
                        };
                        for &(x, y, _) in a.iter().chain(&b) {
                            worst = worst.max(at(&a, x, y).abs_diff(at(&b, x, y)));
                        }
                        masks += 1;
                    }
                }
            }
        }
        assert!(masks > 900);
        assert!(
            worst <= 2,
            "swash and DirectWrite masks differ by {worst} codes"
        );
        Ok(())
    }
}
