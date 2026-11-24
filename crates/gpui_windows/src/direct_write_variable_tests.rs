//! vue-native fork: a variable font's axes follow the font asked for, and the face that is
//! measured is the one drawn: DirectWrite with the axes set explicitly is the reference.

use super::*;
use core::prelude::v1::test;

const MANROPE: &[u8] = include_bytes!("../../../assets/fonts/manrope/Manrope-latin-wght.ttf");
const FLEX: &[u8] = include_bytes!("../../../assets/fonts/roboto-flex/RobotoFlex-axes.ttf");
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

fn axis(tag: &[u8; 4], value: f32) -> DWRITE_FONT_AXIS_VALUE {
    DWRITE_FONT_AXIS_VALUE {
        axisTag: DWRITE_FONT_AXIS_TAG(axis_tag(tag).swap_bytes()),
        value,
    }
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

/// The width of `TEXT` in a layout that asks DirectWrite for the family with `axes` set.
fn directwrite_width(
    ts: &DirectWriteTextSystem,
    family: &HSTRING,
    collection: &IDWriteFontCollection1,
    axes: &[DWRITE_FONT_AXIS_VALUE],
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
        format.SetFontAxisValues(axes)?;
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

#[derive(Debug)]
struct Drawn {
    width: f32,
    reference_width: f32,
    ink: u64,
    reference_ink: u64,
    skathi_ink: u64,
    skathi_reference_ink: u64,
    mismatched_masks: usize,
    /// How far right the ink of the top third of the first glyph sits from that of its bottom
    /// third, in pixels: positive leans forward.
    lean: f32,
}

fn lean(bounds: Bounds<DevicePixels>, mask: &[u8]) -> f32 {
    let (w, h) = (bounds.size.width.0 as usize, bounds.size.height.0 as usize);
    let centroid = |rows: std::ops::Range<usize>| {
        let (mut sum, mut total) = (0.0, 0.0);
        for y in rows {
            for x in 0..w {
                let v = mask[y * w + x] as f32;
                sum += v * x as f32;
                total += v;
            }
        }
        sum / total.max(1.0)
    };
    centroid(0..h / 3) - centroid(h - h / 3..h)
}

/// `font` laid out and drawn, against DirectWrite with `axes` set on the font's resource.
fn drawn(
    ts: &DirectWriteTextSystem,
    font: &Font,
    axes: &[DWRITE_FONT_AXIS_VALUE],
    slot: usize,
) -> Result<Drawn> {
    let font_id = ts.font_id(font)?;
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
        let font_id = layout.runs[0].font_id;
        let info = &state.fonts[font_id.0];
        let resource = info.font_resource.clone().context("a variable resource")?;
        let face: IDWriteFontFace3 =
            unsafe { resource.CreateFontFace(DWRITE_FONT_SIMULATIONS_NONE, axes)? }.cast()?;
        let width = directwrite_width(ts, &info.font_family_h, &info.font_collection, axes)?;
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
        lean: f32::NAN,
    };
    for run in &layout.runs {
        for glyph in &run.glyphs {
            let params = params(run.font_id, glyph.id);
            let state = ts.state.read();
            let face = state.shaping_face(&params);
            let ours = directwrite_mask(&ts.components, face, glyph.id)?;
            let reference = directwrite_mask(&ts.components, &reference_face, glyph.id)?;
            if row.lean.is_nan() {
                row.lean = lean(ours.0, &ours.1);
            }
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
                font_id: FontId(usize::MAX - slot),
                ..params
            };
            row.skathi_reference_ink += skathi(&reference_face, &reference_params)?;
        }
    }
    Ok(row)
}

fn assert_matches(label: &str, row: &Drawn) {
    assert!(
        (row.width - row.reference_width).abs() < 0.01,
        "{label}: width {} against {}",
        row.width,
        row.reference_width
    );
    assert_eq!(
        row.mismatched_masks, 0,
        "{label}: masks differ from the instance's"
    );
    assert_eq!(
        row.skathi_ink, row.skathi_reference_ink,
        "{label}: skathi-mode"
    );
}

fn family_of(ts: &DirectWriteTextSystem, prefix: &str) -> Result<String> {
    ts.all_font_names()
        .into_iter()
        .filter(|name| name.starts_with(prefix))
        .min_by_key(|name| name.len())
        .context("the fixture's family")
}

fn width_of(ts: &DirectWriteTextSystem, font: &Font) -> Result<f32> {
    let run = FontRun {
        len: TEXT.len(),
        font_id: ts.font_id(font)?,
        tracking: px(0.),
        word_spacing: px(0.),
    };
    Ok(ts.layout_line(TEXT, px(SIZE), &[run]).width.as_f32())
}

#[test]
fn a_variable_font_is_drawn_at_the_weight_asked_for() -> Result<()> {
    let ts = DirectWriteTextSystem::new()?;
    ts.add_fonts(vec![Cow::Borrowed(MANROPE)])?;
    let family = family_of(&ts, "Manrope")?;
    let mut rows = Vec::new();
    for (i, weight) in WEIGHTS.into_iter().enumerate() {
        let font = Font {
            family: family.clone().into(),
            weight: FontWeight(weight),
            ..Default::default()
        };
        rows.push(drawn(&ts, &font, &[axis(b"wght", weight)], i)?);
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
        assert_matches(&weight.to_string(), row);
    }

    // A `unicode-range` subset declared `font-weight: 300 700` draws 800 at 700.
    let face_family: SharedString = family.clone().into();
    let composer: FamilyComposer = Arc::new(move |_: &Font| {
        vec![ComposedFace {
            family: face_family.clone(),
            ranges: Arc::from([(0x0, 0xFF)]),
            descriptors: FaceDescriptors {
                weight: Some((FontWeight(300.0), FontWeight(700.0))),
                ..Default::default()
            },
        }]
    });
    ts.set_composed_families(vec![("VF Web".into(), composer)]);
    let width = |family: &str, weight: f32| {
        width_of(
            &ts,
            &Font {
                family: family.to_string().into(),
                weight: FontWeight(weight),
                ..Default::default()
            },
        )
    };
    assert_eq!(width("VF Web", 800.0)?, width(&family, 700.0)?);
    assert_eq!(width("VF Web", 450.0)?, width(&family, 450.0)?);
    assert_eq!(width("VF Web", 200.0)?, width(&family, 300.0)?);
    Ok(())
}

#[test]
fn each_axis_of_a_variable_font_follows_the_font_asked_for() -> Result<()> {
    let ts = DirectWriteTextSystem::new()?;
    ts.add_fonts(vec![Cow::Borrowed(FLEX)])?;
    let family: SharedString = family_of(&ts, "Roboto Flex Axes")?.into();
    let base = Font {
        family: family.clone(),
        ..Default::default()
    };
    let at = |wght: f32, wdth: f32, slnt: f32, grad: f32| {
        [
            axis(b"wght", wght),
            axis(b"wdth", wdth),
            axis(b"slnt", slnt),
            axis(b"GRAD", grad),
        ]
    };
    let oblique = |angle: f32| Font {
        style: FontStyle::Oblique,
        oblique_angle: ObliqueAngle(angle),
        ..base.clone()
    };
    let wide = |width: f32| Font {
        width: FontWidth(width),
        ..base.clone()
    };
    let set = |settings: &[(&[u8; 4], f32)], font: Font| Font {
        variations: FontVariations::new(settings.iter().map(|(t, v)| (axis_tag(t), *v))),
        ..font
    };
    let cases: Vec<(&str, Font, [DWRITE_FONT_AXIS_VALUE; 4])> = vec![
        ("normal", base.clone(), at(400., 100., 0., 0.)),
        ("condensed", wide(75.), at(400., 75., 0., 0.)),
        ("expanded", wide(125.), at(400., 125., 0., 0.)),
        ("10% clamps to the axis", wide(10.), at(400., 25., 0., 0.)),
        ("oblique 8deg", oblique(8.), at(400., 100., -8., 0.)),
        (
            "italic through slnt",
            Font {
                style: FontStyle::Italic,
                ..base.clone()
            },
            at(400., 100., -10., 0.),
        ),
        ("oblique -5deg clamps", oblique(-5.), at(400., 100., 0., 0.)),
        (
            "GRAD 150",
            set(&[(b"GRAD", 150.)], base.clone()),
            at(400., 100., 0., 150.),
        ),
        (
            "wght setting over bold",
            set(
                &[(b"wght", 300.)],
                Font {
                    weight: FontWeight::BOLD,
                    ..base.clone()
                },
            ),
            at(300., 100., 0., 0.),
        ),
        (
            "wdth setting over expanded",
            set(&[(b"wdth", 60.)], wide(125.)),
            at(400., 60., 0., 0.),
        ),
    ];
    let mut rows = Vec::new();
    eprintln!(
        "case | width (DirectWrite, axes set) | ink (axes set) | skathi-mode ink | masks off | lean px"
    );
    for (i, (label, font, axes)) in cases.iter().enumerate() {
        let row = drawn(&ts, font, axes, 100 + i)?;
        eprintln!(
            "{label} | {:8.3} ({:8.3}) | {:7} ({:7}) | {:7} ({:7}) | {} | {:.2}",
            row.width,
            row.reference_width,
            row.ink,
            row.reference_ink,
            row.skathi_ink,
            row.skathi_reference_ink,
            row.mismatched_masks,
            row.lean
        );
        assert_matches(label, &row);
        rows.push(row);
    }
    let [
        normal,
        condensed,
        expanded,
        clamped,
        oblique8,
        italic,
        backward,
        grad,
        wght,
        wdth,
    ] = &rows[..]
    else {
        unreachable!()
    };
    assert!(condensed.width < normal.width && normal.width < expanded.width);
    assert!(clamped.width < condensed.width);
    assert!(oblique8.lean > normal.lean + 1.0, "oblique leans forward");
    assert!(italic.lean > oblique8.lean, "italic is slnt -10");
    assert_eq!(backward.ink, normal.ink);
    assert!(grad.ink > normal.ink);
    assert!(wght.width < normal.width);
    assert!(wdth.width < condensed.width);

    // A face declared `font-stretch: 75% 100%`, `font-style: oblique 0deg 5deg` and
    // `font-variation-settings: "GRAD" 100`, which the run's own settings override.
    let face_family = family.clone();
    let composer: FamilyComposer = Arc::new(move |_: &Font| {
        vec![ComposedFace {
            family: face_family.clone(),
            ranges: Arc::from([]),
            descriptors: FaceDescriptors {
                width: Some((FontWidth(75.), FontWidth(100.))),
                oblique: Some((ObliqueAngle(0.), ObliqueAngle(5.))),
                variations: FontVariations::new([(axis_tag(b"GRAD"), 100.)]),
                ..Default::default()
            },
        }]
    });
    ts.set_composed_families(vec![("Flex Web".into(), composer)]);
    let web = |font: Font| Font {
        family: "Flex Web".into(),
        ..font
    };
    let clamped = drawn(&ts, &web(wide(125.)), &at(400., 100., 0., 100.), 200)?;
    assert_matches("descriptor width", &clamped);
    let clamped = drawn(&ts, &web(oblique(12.)), &at(400., 100., -5., 100.), 201)?;
    assert_matches("descriptor oblique", &clamped);
    let overridden = drawn(
        &ts,
        &web(set(&[(b"GRAD", -100.)], base.clone())),
        &at(400., 100., 0., -100.),
        202,
    )?;
    assert_matches("descriptor settings", &overridden);
    Ok(())
}
