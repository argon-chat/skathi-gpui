use super::DirectWriteTextSystem;
use anyhow::Result;
use gpui::{
    ComposedFace, FamilyComposer, Font, FontFallbacks, FontId, GlyphId, TextRun, TextSystem,
    WindowTextSystem, px,
};
use std::sync::Arc;

fn font(family: &str) -> Font {
    Font {
        family: family.to_string().into(),
        ..Default::default()
    }
}

fn listed(family: &str, fallbacks: &[&str]) -> Font {
    Font {
        fallbacks: Some(FontFallbacks::from_fonts(
            fallbacks.iter().map(|f| f.to_string()).collect(),
        )),
        ..font(family)
    }
}

/// "VN Han": SimSun for the CJK Unified Ideographs. "VN Wide": Consolas for every code point,
/// as a web face without `unicode-range` is.
fn text_systems() -> Result<(Arc<TextSystem>, WindowTextSystem)> {
    let platform = Arc::new(DirectWriteTextSystem::new()?);
    let text_system = Arc::new(TextSystem::new(platform));
    let han: FamilyComposer = Arc::new(|_, _| {
        vec![ComposedFace {
            family: "SimSun".into(),
            ranges: Arc::from([(0x4E00, 0x9FFF)]),
        }]
    });
    let wide: FamilyComposer = Arc::new(|_, _| {
        vec![ComposedFace {
            family: "Consolas".into(),
            ranges: Arc::from([]),
        }]
    });
    text_system.set_composed_families(vec![("VN Han".into(), han), ("VN Wide".into(), wide)]);
    let window = WindowTextSystem::new(text_system.clone());
    Ok((text_system, window))
}

/// The font and glyph each character of `text` was drawn with.
fn drawn_with(window: &WindowTextSystem, text: &str, font: &Font) -> Vec<(char, FontId, GlyphId)> {
    let runs = [TextRun {
        len: text.len(),
        font: font.clone(),
        ..Default::default()
    }];
    let shaped = window.shape_line(text.to_string().into(), px(16.0), &runs, None);
    shaped
        .runs
        .iter()
        .flat_map(|run| {
            run.glyphs.iter().map(|glyph| {
                (
                    text[glyph.index..].chars().next().unwrap(),
                    run.font_id,
                    glyph.id,
                )
            })
        })
        .collect()
}

fn fonts_of(drawn: &[(char, FontId, GlyphId)]) -> Vec<(char, FontId)> {
    drawn.iter().map(|&(c, font_id, _)| (c, font_id)).collect()
}

#[test]
fn a_web_family_after_an_installed_one_draws_what_that_one_lacks() -> Result<()> {
    let (text_system, window) = text_systems()?;
    let segoe = text_system.resolve_font(&font("Segoe UI"));
    let simsun = text_system.resolve_font(&font("SimSun"));
    let drawn = drawn_with(&window, "a\u{8BBE}", &listed("Segoe UI", &["VN Han"]));
    assert_eq!(fonts_of(&drawn), vec![('a', segoe), ('\u{8BBE}', simsun)]);
    Ok(())
}

#[test]
fn a_face_without_the_glyph_leaves_the_character_to_the_next_family() -> Result<()> {
    let (text_system, window) = text_systems()?;
    let consolas = text_system.resolve_font(&font("Consolas"));
    let simsun = text_system.resolve_font(&font("SimSun"));
    // Consolas's range is everything, but it has no glyph for U+8BBE.
    let drawn = drawn_with(&window, "a\u{8BBE}", &listed("VN Wide", &["SimSun"]));
    assert_eq!(
        fonts_of(&drawn),
        vec![('a', consolas), ('\u{8BBE}', simsun)]
    );
    Ok(())
}

#[test]
fn a_character_no_family_has_is_the_first_familys_missing_glyph() -> Result<()> {
    let (text_system, window) = text_systems()?;
    let segoe = text_system.resolve_font(&font("Segoe UI"));
    // Plane 16's private use: no face of the list, nor any DirectWrite falls back to, has it.
    let drawn = drawn_with(
        &window,
        "a\u{10FFFD}",
        &listed("Segoe UI", &["Consolas", "VN Han"]),
    );
    assert_eq!(drawn[0].1, segoe);
    assert_eq!(drawn[1], ('\u{10FFFD}', segoe, GlyphId(0)));
    Ok(())
}
