//! vue-native fork: a variable font's `wght` axis follows the weight asked for, and the face
//! that is measured is the one drawn: DirectWrite with the axis set explicitly is the reference.

use super::*;
use core::prelude::v1::test;

const MANROPE: &[u8] = include_bytes!("../../../assets/fonts/manrope/Manrope-latin-wght.ttf");
const TEXT: &str = "Hamburgefonstiv";
const SIZE: f32 = 20.0;
const WEIGHTS: [f32; 6] = [200.0, 300.0, 400.0, 450.0, 600.0, 800.0];

fn params(font_id: FontId, glyph: GlyphId) -> RenderGlyphParams {
    RenderGlyphParams {
        font_id,
        glyph_id: glyph,
        font_size: px(SIZE),
        subpixel_variant: Default::default(),
        scale_factor: 1.0,
        is_emoji: false,
        subpixel_rendering: false,
        dilation: 0,
    }
}

fn mask_ink(mask: &[u8]) -> u64 {
    mask.iter().map(|&v| v as u64).sum()
}

/// The glyph's grayscale mask as DirectWrite draws `face` itself.
fn directwrite_mask(
    components: &DirectWriteComponents,
    face: &IDWriteFontFace3,
    glyph: GlyphId,
) -> Result<(Bounds<DevicePixels>, Vec<u8>)> {
    let glyph_id = [glyph.0 as u16];
    let advance = [0.0];
    let offset = [DWRITE_GLYPH_OFFSET::default()];
    let run_face: &IDWriteFontFace = face;
    let run = DWRITE_GLYPH_RUN {
        fontFace: ManuallyDrop::new(Some(unsafe { std::ptr::read(run_face) })),
        fontEmSize: SIZE,
        glyphCount: 1,
        glyphIndices: glyph_id.as_ptr(),
        glyphAdvances: advance.as_ptr(),
        glyphOffsets: offset.as_ptr(),
        isSideways: BOOL(0),
        bidiLevel: 0,
    };
    unsafe {
        let mut mode = DWRITE_RENDERING_MODE1::default();
        let mut grid = DWRITE_GRID_FIT_MODE::default();
        face.GetRecommendedRenderingMode(
            SIZE,
            96.0,
            96.0,
            None,
            false,
            DWRITE_OUTLINE_THRESHOLD_ANTIALIASED,
            DWRITE_MEASURING_MODE_NATURAL,
            None,
            &mut mode,
            &mut grid,
        )?;
        if mode == DWRITE_RENDERING_MODE1_OUTLINE {
            mode = DWRITE_RENDERING_MODE1_NATURAL_SYMMETRIC;
        }
        let analysis = components.factory.CreateGlyphRunAnalysis(
            &run,
            None,
            mode,
            DWRITE_MEASURING_MODE_NATURAL,
            grid,
            DWRITE_TEXT_ANTIALIAS_MODE_GRAYSCALE,
            0.0,
            0.0,
        )?;
        let rect = analysis.GetAlphaTextureBounds(DWRITE_TEXTURE_ALIASED_1x1)?;
        let mut bytes = vec![0u8; ((rect.right - rect.left) * (rect.bottom - rect.top)) as usize];
        if !bytes.is_empty() {
            analysis.CreateAlphaTexture(DWRITE_TEXTURE_ALIASED_1x1, &rect, &mut bytes)?;
        }
        let bounds = Bounds::from_corners(
            point(rect.left.into(), rect.top.into()),
            point(rect.right.into(), rect.bottom.into()),
        );
        Ok((bounds, bytes))
    }
}

/// The width of `TEXT` in a layout that asks DirectWrite for the family with `wght` set.
fn directwrite_width(
    ts: &DirectWriteTextSystem,
    family: &HSTRING,
    collection: &IDWriteFontCollection1,
    weight: f32,
) -> Result<f32> {
    unsafe {
        let format: IDWriteTextFormat3 = ts
            .components
            .factory
            .CreateTextFormat(
                family,
                collection,
                DWRITE_FONT_WEIGHT_NORMAL,
                DWRITE_FONT_STYLE_NORMAL,
                DWRITE_FONT_STRETCH_NORMAL,
                SIZE,
                &ts.components.locale,
            )?
            .cast()?;
        format.SetFontAxisValues(&[DWRITE_FONT_AXIS_VALUE {
            axisTag: DWRITE_FONT_AXIS_TAG_WEIGHT,
            value: weight,
        }])?;
        let wide: Vec<u16> = TEXT.encode_utf16().collect();
        let layout =
            ts.components
                .factory
                .CreateTextLayout(&wide, &format, f32::INFINITY, f32::INFINITY)?;
        let mut metrics = DWRITE_TEXT_METRICS::default();
        layout.GetMetrics(&mut metrics)?;
        Ok(metrics.widthIncludingTrailingWhitespace)
    }
}

struct Drawn {
    width: f32,
    reference_width: f32,
    ink: u64,
    reference_ink: u64,
    skathi_ink: u64,
    skathi_reference_ink: u64,
    mismatched_masks: usize,
}

#[test]
fn a_variable_font_is_drawn_at_the_weight_asked_for() -> Result<()> {
    let ts = DirectWriteTextSystem::new()?;
    ts.add_fonts(vec![Cow::Borrowed(MANROPE)])?;
    let family = ts
        .all_font_names()
        .into_iter()
        .filter(|name| name.starts_with("Manrope"))
        .min_by_key(|name| name.len())
        .context("the fixture's family")?;
    let mut rows = Vec::new();
    for (i, weight) in WEIGHTS.into_iter().enumerate() {
        let font = Font {
            family: family.clone().into(),
            weight: FontWeight(weight),
            ..Default::default()
        };
        let font_id = ts.font_id(&font)?;
        let layout = ts.layout_line(
            TEXT,
            px(SIZE),
            &[FontRun {
                len: TEXT.len(),
                font_id,
                tracking: px(0.),
                word_spacing: px(0.),
            }],
        );
        let (reference_face, reference_width) = {
            let state = ts.state.read();
            let info = &state.fonts[font_id.0];
            let resource = info.font_resource.clone().context("a variable resource")?;
            let face: IDWriteFontFace3 = unsafe {
                resource.CreateFontFace(
                    DWRITE_FONT_SIMULATIONS_NONE,
                    &[DWRITE_FONT_AXIS_VALUE {
                        axisTag: DWRITE_FONT_AXIS_TAG_WEIGHT,
                        value: weight,
                    }],
                )?
            }
            .cast()?;
            let width = directwrite_width(&ts, &info.font_family_h, &info.font_collection, weight)?;
            (face, width)
        };
        let mut row = Drawn {
            width: layout.width.as_f32(),
            reference_width,
            ink: 0,
            reference_ink: 0,
            skathi_ink: 0,
            skathi_reference_ink: 0,
            mismatched_masks: 0,
        };
        for run in &layout.runs {
            for glyph in &run.glyphs {
                let params = params(run.font_id, glyph.id);
                let state = ts.state.read();
                let face = state.shaping_face(&params);
                let ours = directwrite_mask(&ts.components, face, glyph.id)?;
                let reference = directwrite_mask(&ts.components, &reference_face, glyph.id)?;
                row.ink += mask_ink(&ours.1);
                row.reference_ink += mask_ink(&reference.1);
                if ours != reference {
                    row.mismatched_masks += 1;
                }
                let skathi = |face: &IDWriteFontFace3, params: &RenderGlyphParams| -> Result<u64> {
                    let bounds = crate::skathi_raster::raster_bounds(face, params)?;
                    if bounds.size.width.0 == 0 {
                        return Ok(0);
                    }
                    Ok(mask_ink(&crate::skathi_raster::rasterize(
                        face, params, bounds,
                    )?))
                };
                row.skathi_ink += skathi(face, &params)?;
                let reference_params = RenderGlyphParams {
                    font_id: FontId(usize::MAX - i),
                    ..params
                };
                row.skathi_reference_ink += skathi(&reference_face, &reference_params)?;
            }
        }
        rows.push(row);
    }

    eprintln!(
        "wght | width (DirectWrite, wght set) | ink (wght set) | skathi-mode ink (wght set) | masks off"
    );
    for (weight, row) in WEIGHTS.iter().zip(&rows) {
        eprintln!(
            "{weight:>4} | {:8.3} ({:8.3}) | {:7} ({:7}) | {:7} ({:7}) | {}",
            row.width,
            row.reference_width,
            row.ink,
            row.reference_ink,
            row.skathi_ink,
            row.skathi_reference_ink,
            row.mismatched_masks
        );
    }
    for pair in rows.windows(2) {
        assert!(
            pair[1].width > pair[0].width,
            "widths do not grow with the weight"
        );
        assert!(
            pair[1].ink > pair[0].ink,
            "ink does not grow with the weight"
        );
        assert!(
            pair[1].skathi_ink > pair[0].skathi_ink,
            "skathi-mode ink does not grow"
        );
    }
    for (weight, row) in WEIGHTS.iter().zip(&rows) {
        assert!(
            (row.width - row.reference_width).abs() < 0.01,
            "{weight}: width {} against {}",
            row.width,
            row.reference_width
        );
        assert_eq!(
            row.mismatched_masks, 0,
            "{weight}: masks differ from the instance's"
        );
        assert_eq!(
            row.skathi_ink, row.skathi_reference_ink,
            "{weight}: skathi-mode"
        );
    }

    // A `unicode-range` subset declared `font-weight: 300 700` draws 800 at 700.
    let face_family: SharedString = family.clone().into();
    let composer: FamilyComposer = Arc::new(move |weight: FontWeight, _| {
        vec![ComposedFace {
            family: face_family.clone(),
            ranges: Arc::from([(0x0, 0xFF)]),
            weight: Some(FontWeight(weight.0.clamp(300.0, 700.0))),
        }]
    });
    ts.set_composed_families(vec![("VF Web".into(), composer)]);
    let width = |family: &str, weight: f32| -> Result<f32> {
        let font_id = ts.font_id(&Font {
            family: family.to_string().into(),
            weight: FontWeight(weight),
            ..Default::default()
        })?;
        let run = FontRun {
            len: TEXT.len(),
            font_id,
            tracking: px(0.),
            word_spacing: px(0.),
        };
        Ok(ts.layout_line(TEXT, px(SIZE), &[run]).width.as_f32())
    };
    assert_eq!(width("VF Web", 800.0)?, width(&family, 700.0)?);
    assert_eq!(width("VF Web", 450.0)?, width(&family, 450.0)?);
    assert_eq!(width("VF Web", 200.0)?, width(&family, 300.0)?);
    Ok(())
}
