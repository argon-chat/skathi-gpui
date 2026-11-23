//! Skathi's CoreText path against what a native label draws: the font AppKit hands out for the
//! same point size and weight, drawn by CoreText with the label's raster settings and the real
//! foreground colour. Expectations come from the system at test time; nothing of the system
//! font is stored (its licence does not allow it). `-- --nocapture` prints the tables.

use super::*;
use crate::metal_renderer::MetalRenderer;
use core_foundation::{dictionary::CFDictionary, dictionary::CFDictionaryRef};
use core_graphics::context::CGContext;
use core_text::font::CTFontRef;
use gpui::{
    AtlasKey, ContentMask, FontRun, MonochromeSprite, PlatformAtlas as _, Quad, ScaledPixels,
    Scene, TransformationMatrix, font, rgb,
};
use objc2::{
    msg_send,
    runtime::{AnyClass, AnyObject},
};

#[link(name = "AppKit", kind = "framework")]
unsafe extern "C" {}

unsafe extern "C" {
    fn CTFontCopyVariation(font: CTFontRef) -> CFDictionaryRef;
}

const OPSZ: i64 = 0x6f70737a;
const WGHT: i64 = 0x77676874;

/// `NSFont.systemFont(ofSize:weight:)`, what an AppKit or SwiftUI label sets its text in.
fn native_system_font(points: f64, css_weight: f32) -> CTFont {
    let weight: f64 = match css_weight as u32 {
        100 => -0.8,
        200 => -0.6,
        300 => -0.4,
        500 => 0.23,
        600 => 0.3,
        700 => 0.4,
        800 => 0.56,
        900 => 0.62,
        _ => 0.0,
    };
    let class = AnyClass::get(c"NSFont").expect("AppKit is linked");
    let font: *mut AnyObject =
        unsafe { msg_send![class, systemFontOfSize: points, weight: weight] };
    assert!(!font.is_null());
    // SAFETY: NSFont is toll-free bridged with CTFont; the object is autoreleased, so retain it.
    unsafe { CTFont::wrap_under_get_rule(font as CTFontRef) }
}

fn variation(font: &CTFont, axis: i64) -> Option<f64> {
    let dictionary = unsafe { CTFontCopyVariation(font.as_concrete_TypeRef()) };
    if dictionary.is_null() {
        return None;
    }
    let dictionary: CFDictionary<CFNumber, CFNumber> =
        unsafe { CFDictionary::wrap_under_create_rule(dictionary) };
    dictionary
        .find(CFNumber::from(axis))
        .and_then(|value| value.to_f64())
}

fn srgb(color: u32) -> [f64; 3] {
    [16, 8, 0].map(|shift| ((color >> shift) & 0xff) as f64 / 255.0)
}

fn decode(u: f64) -> f64 {
    if u <= 0.04045 {
        u / 12.92
    } else {
        ((u + 0.055) / 1.055).powf(2.4)
    }
}

/// The level of the law in the notes: linear luminance to the nearest quarter.
fn macos_level(color: [f64; 3]) -> f64 {
    let y = 0.2126 * decode(color[0]) + 0.7152 * decode(color[1]) + 0.0722 * decode(color[2]);
    (4.0 * y).round() / 4.0
}

fn hsla(color: u32) -> Hsla {
    rgb(color).into()
}

/// A canvas of device pixels, rows top down, with the pen's baseline at `(x0, y0)`.
struct Canvas {
    width: usize,
    height: usize,
    x0: i32,
    y0: i32,
    pixels: Vec<u8>,
    placed: Option<(i32, i32, i32, i32)>,
}

impl Canvas {
    fn for_size(ppem: f32, columns: f32) -> Self {
        let width = (ppem * columns).ceil() as usize + 32;
        let height = (ppem * 2.5).ceil() as usize + 16;
        Canvas {
            width,
            height,
            x0: 12,
            y0: (ppem * 1.6).ceil() as i32 + 8,
            pixels: vec![0; width * height],
            placed: None,
        }
    }

    fn context(&mut self, scale: f64) -> CGContext {
        let cx = CGContext::create_bitmap_context(
            Some(self.pixels.as_mut_ptr() as *mut _),
            self.width,
            self.height,
            8,
            self.width,
            &CGColorSpace::create_device_gray(),
            kCGImageAlphaOnly,
        );
        cx.translate(
            self.x0 as CGFloat,
            (self.height as i32 - self.y0) as CGFloat,
        );
        cx.scale(scale, scale);
        cx
    }

    /// Draws with what a native label asks for: antialiasing, smoothing, subpixel positioning
    /// and quantisation, the foreground in sRGB.
    fn native_settings(cx: &CGContext, color: [f64; 3], smooth: bool) {
        cx.set_allows_antialiasing(true);
        cx.set_should_antialias(true);
        cx.set_allows_font_smoothing(true);
        cx.set_should_smooth_fonts(smooth);
        cx.set_allows_font_subpixel_positioning(true);
        cx.set_should_subpixel_position_fonts(true);
        cx.set_allows_font_subpixel_quantization(true);
        cx.set_should_subpixel_quantize_fonts(true);
        cx.set_fill_color(&srgb_color(color[0], color[1], color[2]));
    }

    /// One glyph as CoreText draws it, its origin `x` device pixels right of the pen.
    fn draw_native_glyph(
        &mut self,
        font: &CTFont,
        glyph: u32,
        x: f64,
        scale: f64,
        color: [f64; 3],
    ) {
        self.draw_native_glyph_with(font, glyph, x, scale, color, true);
    }

    fn draw_native_glyph_with(
        &mut self,
        font: &CTFont,
        glyph: u32,
        x: f64,
        scale: f64,
        color: [f64; 3],
        smooth: bool,
    ) {
        let cx = self.context(scale);
        Self::native_settings(&cx, color, smooth);
        font.draw_glyphs(&[glyph as CGGlyph], &[CGPoint::new(x / scale, 0.0)], cx);
    }

    /// A whole line as CoreText lays it out and draws it.
    fn draw_native_line(&mut self, font: &CTFont, text: &str, scale: f64, color: [f64; 3]) {
        let line = native_line(font, text);
        let cx = self.context(scale);
        Self::native_settings(&cx, color, true);
        cx.set_text_position(0.0, 0.0);
        line.draw(&cx);
    }

    /// Skathi's glyph: its origin quantised as the window does, rasterised by our path.
    fn draw_our_glyph(&mut self, ts: &AppleTextSystem, mut params: RenderGlyphParams, x: f32) {
        let origin = gpui::point(
            ScaledPixels(self.x0 as f32 + x),
            ScaledPixels(self.y0 as f32),
        );
        let ppem = f32::from(params.font_size) * params.scale_factor;
        let (whole, variant) = gpui::quantize_glyph_origin(origin, ppem);
        params.subpixel_variant = variant;
        let bounds = ts.glyph_raster_bounds(&params).unwrap();
        if bounds.size.width.0 == 0 {
            return;
        }
        let (size, mask) = ts.rasterize_glyph(&params, bounds).unwrap();
        let left = whole.x.0 as i32 + bounds.origin.x.0;
        let top = whole.y.0 as i32 + bounds.origin.y.0;
        self.placed = Some((left, top, size.width.0, size.height.0));
        for row in 0..size.height.0 {
            for column in 0..size.width.0 {
                let (x, y) = (left + column, top + row);
                assert!(
                    x >= 0 && y >= 0 && (x as usize) < self.width && (y as usize) < self.height,
                    "glyph outside the canvas"
                );
                let value = mask[(row * size.width.0 + column) as usize];
                let pixel = &mut self.pixels[y as usize * self.width + x as usize];
                *pixel = (*pixel).max(value);
            }
        }
    }

    fn ink(&self) -> f64 {
        self.pixels.iter().map(|&v| v as f64).sum::<f64>() / 255.0
    }
}

fn native_line(font: &CTFont, text: &str) -> CTLine {
    let mut string = CFMutableAttributedString::new();
    string.replace_str(&CFString::new(text), CFRange::init(0, 0));
    let range = CFRange::init(0, string.char_len());
    unsafe {
        string.set_attribute(range, kCTFontAttributeName, font);
        // Without it CTLineDraw fills with the string's foreground, black, not the context's.
        string.set_attribute(
            range,
            core_text::string_attributes::kCTForegroundColorFromContextAttributeName,
            &core_foundation::boolean::CFBoolean::true_value(),
        );
    }
    CTLine::new_with_attributed_string(string.as_concrete_TypeRef())
}

#[derive(Default)]
struct Diff {
    samples: usize,
    mean_sum: f64,
    max: u8,
    over_gate: usize,
    ink_ours: f64,
    ink_native: f64,
}

impl Diff {
    fn add(&mut self, ours: &Canvas, native: &Canvas) {
        let (mut sum, mut count, mut max) = (0u64, 0u64, 0u8);
        for (&a, &b) in ours.pixels.iter().zip(&native.pixels) {
            if a > 0 || b > 0 {
                sum += a.abs_diff(b) as u64;
                count += 1;
            }
            max = max.max(a.abs_diff(b));
        }
        self.samples += 1;
        self.mean_sum += if count == 0 {
            0.0
        } else {
            sum as f64 / count as f64
        };
        self.max = self.max.max(max);
        self.over_gate += (max > 16) as usize;
        self.ink_ours += ours.ink();
        self.ink_native += native.ink();
    }

    fn row(&self) -> String {
        format!(
            "ink {:+6.2}% | mean {:5.2} | max {:3} | over 16: {:3}/{}",
            100.0 * (self.ink_ours / self.ink_native - 1.0),
            self.mean_sum / self.samples as f64,
            self.max,
            self.over_gate,
            self.samples
        )
    }
}

fn system_font_id(ts: &AppleTextSystem, weight: f32) -> FontId {
    let mut system = font(".SystemUIFont");
    system.weight = FontWeight(weight);
    ts.font_id(&system).unwrap()
}

fn run(font_id: FontId, text: &str) -> FontRun {
    FontRun {
        font_id,
        len: text.len(),
        tracking: px(0.),
        word_spacing: px(0.),
    }
}

/// The glyphs of `text` with their pen offsets in points, as our layout shapes them.
fn our_glyphs(
    ts: &AppleTextSystem,
    font_id: FontId,
    text: &str,
    size: f32,
) -> Vec<(FontId, u32, f32)> {
    let layout = ts.layout_line(text, px(size), &[run(font_id, text)]);
    layout
        .runs
        .iter()
        .flat_map(|run| {
            run.glyphs
                .iter()
                .map(move |glyph| (run.font_id, glyph.id.0, f32::from(glyph.position.x)))
        })
        .collect()
}

fn params(font_id: FontId, glyph: u32, size: f32, scale: f32, dilation: u8) -> RenderGlyphParams {
    RenderGlyphParams {
        font_id,
        glyph_id: GlyphId(glyph),
        font_size: px(size),
        subpixel_variant: Default::default(),
        scale_factor: scale,
        is_emoji: false,
        subpixel_rendering: false,
        dilation,
    }
}

const LADDER: [u32; 21] = [
    0x000000, 0x0d0d0d, 0x1a1a1a, 0x262626, 0x333333, 0x404040, 0x4d4d4d, 0x595959, 0x666666,
    0x737373, 0x808080, 0x8c8c8c, 0x999999, 0xa6a6a6, 0xb3b3b3, 0xbfbfbf, 0xcccccc, 0xd9d9d9,
    0xe6e6e6, 0xf2f2f2, 0xffffff,
];
const PRIMARIES: [u32; 5] = [0xff0000, 0x00ff00, 0x0000ff, 0x0a84ff, 0xff3b30];

/// #103: the mask our path rasterises for a colour carries the dilation CoreText gives that
/// colour. Same font object on both sides, phase zero, so only the dilation level differs.
#[test]
fn dilation_level_matches_coretext() {
    let ts = AppleTextSystem::new();
    let font_id = system_font_id(&ts, 400.);
    let mut worst = 0;
    for (size, scale) in [(13.0f32, 1.0f32), (13.0, 2.0)] {
        let glyphs = our_glyphs(&ts, font_id, "aeHgo", size);
        eprintln!(
            "\n#103 dilation, {size} px at {scale}x ({} ppem), glyphs aeHgo",
            size * scale
        );
        eprintln!("colour  | Y lin | q    | level | ours against CoreText with the real colour");
        for color in LADDER.iter().chain(&PRIMARIES).copied() {
            let rgb = srgb(color);
            let level = ts.glyph_dilation_for_color(hsla(color));
            let mut diff = Diff::default();
            for &(font_id, glyph, _) in &glyphs {
                let mut ours = Canvas::for_size(size * scale, 2.0);
                ours.draw_our_glyph(&ts, params(font_id, glyph, size, scale, level), 0.0);
                let mut native = Canvas::for_size(size * scale, 2.0);
                let native_font = ts.0.read().fonts[font_id.0]
                    .native_font()
                    .clone_with_font_size(size as f64);
                native.draw_native_glyph(&native_font, glyph, 0.0, scale as f64, rgb);
                diff.add(&ours, &native);
            }
            let y = 0.2126 * decode(rgb[0]) + 0.7152 * decode(rgb[1]) + 0.0722 * decode(rgb[2]);
            eprintln!(
                "#{color:06x} | {y:.3} | {:.2} | {level}     | {}",
                macos_level(rgb),
                diff.row()
            );
            worst = worst.max(diff.max);
        }
    }

    // Level 0, smoothing off: `font-smoothing: antialiased`, or smoothing turned off by the user.
    for (size, scale) in [(13.0f32, 1.0f32), (13.0, 2.0)] {
        let mut diff = Diff::default();
        for (font_id, glyph, _) in our_glyphs(&ts, font_id, "aeHgo", size) {
            let native_font = ts.0.read().fonts[font_id.0]
                .native_font()
                .clone_with_font_size(size as f64);
            for color in [0x000000u32, 0xffffff] {
                let mut ours = Canvas::for_size(size * scale, 2.0);
                ours.draw_our_glyph(&ts, params(font_id, glyph, size, scale, 0), 0.0);
                let mut native = Canvas::for_size(size * scale, 2.0);
                native.draw_native_glyph_with(
                    &native_font,
                    glyph,
                    0.0,
                    scale as f64,
                    srgb(color),
                    false,
                );
                diff.add(&ours, &native);
            }
        }
        eprintln!(
            "level 0 at {size} px {scale}x against CoreText unsmoothed | {}",
            diff.row()
        );
        worst = worst.max(diff.max);
    }
    assert!(
        worst <= 1,
        "a dilation level misses CoreText's by {worst} codes"
    );
}

/// #104: line widths, axis coordinates and default features of the system font against a
/// native label's font at the same point size and weight.
#[test]
fn system_font_matches_a_native_label() {
    let ts = AppleTextSystem::new();
    let texts = [
        "Download",
        "Hamburgefonstiv 0123 Wafer",
        "The quick brown fox jumps over the lazy dog. 12:30",
    ];
    let mut failures = Vec::new();
    eprintln!("\n#104 system font: our line width - CTLine width (pt), our opsz/wght vs native");
    for weight in [400.0f32, 500.0, 600.0, 700.0] {
        let font_id = system_font_id(&ts, weight);
        for size in [
            6.0f32, 9.0, 11.0, 13.0, 15.0, 17.0, 20.0, 24.0, 28.0, 34.0, 48.0, 72.0, 96.0,
        ] {
            let native = native_system_font(size as f64, weight);
            let mut deltas = Vec::new();
            for text in texts {
                let ours = ts.layout_line(text, px(size), &[run(font_id, text)]);
                let theirs = native_line(&native, text).get_typographic_bounds().width;
                let delta = f32::from(ours.width) as f64 - theirs;
                deltas.push(delta);
                if delta.abs() > 0.01 {
                    failures.push(format!("{weight} {size}px {text:?}: {delta:+.3} pt"));
                }
            }
            let layout = ts.layout_line("H", px(size), &[run(font_id, "H")]);
            let used = ts.0.read().fonts[layout.runs[0].font_id.0]
                .native_font()
                .clone_with_font_size(size as f64);
            let axes = |font: &CTFont| {
                format!(
                    "opsz {:>5} wght {:>5}",
                    variation(font, OPSZ).map_or("-".into(), |v| format!("{v:.1}")),
                    variation(font, WGHT).map_or("-".into(), |v| format!("{v:.1}"))
                )
            };
            eprintln!(
                "w{weight} {size:>4}px | {} | ours {} {} | native {} {}",
                deltas
                    .iter()
                    .map(|d| format!("{d:+8.3}"))
                    .collect::<Vec<_>>()
                    .join(" "),
                used.postscript_name(),
                axes(&used),
                native.postscript_name(),
                axes(&native),
            );
        }
    }

    let font_id = system_font_id(&ts, 400.);
    for text in ["12:30", "office", "fi fl ffi"] {
        let ours: Vec<u32> = our_glyphs(&ts, font_id, text, 13.)
            .iter()
            .map(|g| g.1)
            .collect();
        let line = native_line(&native_system_font(13., 400.), text);
        let theirs: Vec<u32> = line
            .glyph_runs()
            .iter()
            .flat_map(|run| run.glyphs().iter().map(|&g| g as u32).collect::<Vec<_>>())
            .collect();
        eprintln!("features {text:?}: ours {ours:?} native {theirs:?}");
        if ours != theirs {
            failures.push(format!("{text:?} shapes to {ours:?}, natively {theirs:?}"));
        }
    }

    for family in ["Helvetica Neue", "Avenir Next"] {
        let mut medium = font(family);
        medium.weight = FontWeight(500.);
        let Ok(font_id) = ts.font_id(&medium) else {
            continue;
        };
        let name = ts.0.read().fonts[font_id.0]
            .postscript_name()
            .unwrap_or_default();
        eprintln!("{family} at 500: {name}");
        if !name.contains("Medium") {
            failures.push(format!("{family} at 500 selects {name}"));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// #104: every glyph placed where the window places it, against CoreText drawing the native
/// label's font at the exact pen position. Covers the font instance, the subpixel phase and
/// the autohinting of small sizes.
#[test]
fn glyph_masks_match_coretext_at_every_position() {
    let ts = AppleTextSystem::new();
    let mut worst = 0;
    let corpus: String = (0x21u8..0x7f)
        .map(char::from)
        .chain("ÄÖÜäöüßẞ".chars())
        .collect();
    let mut misses = Vec::new();
    eprintln!(
        "\n#104 glyph masks at 21 pen positions per pixel, printable ASCII and ÄÖÜäöüßẞ, black and white"
    );
    for (size, scale, weight) in [
        (9.0f32, 1.0f32, 400.0f32),
        (11.0, 1.0, 400.),
        (13.0, 1.0, 400.),
        (15.0, 1.0, 400.),
        (18.0, 1.0, 400.),
        (9.0, 2.0, 400.),
        (12.5, 2.0, 400.),
        (13.0, 2.0, 400.),
        (13.0, 2.0, 500.),
        (13.0, 2.0, 600.),
        (17.0, 2.0, 400.),
        (24.0, 2.0, 400.),
        (24.0, 2.0, 700.),
    ] {
        let font_id = system_font_id(&ts, weight);
        let native_font = native_system_font(size as f64, weight);
        let glyphs = our_glyphs(&ts, font_id, &corpus, size);
        for color in [0x000000u32, 0xffffff] {
            let mut diff = Diff::default();
            for &(font_id, glyph, _) in &glyphs {
                for step in 0..=20 {
                    let x = if step == 20 {
                        2.9999999
                    } else {
                        step as f32 / 20.0 + 0.013
                    };
                    let level = ts.glyph_dilation_for_color(hsla(color));
                    let mut ours = Canvas::for_size(size * scale, 2.0);
                    ours.draw_our_glyph(&ts, params(font_id, glyph, size, scale, level), x);
                    let mut native = Canvas::for_size(size * scale, 2.0);
                    native.draw_native_glyph(
                        &native_font,
                        glyph,
                        x as f64,
                        scale as f64,
                        srgb(color),
                    );
                    let before = diff.over_gate;
                    diff.add(&ours, &native);
                    if diff.over_gate > before && misses.len() < 12 {
                        let (mut x0, mut y0, mut x1, mut y1) =
                            (i32::MAX, i32::MAX, i32::MIN, i32::MIN);
                        for (i, (&a, &b)) in ours.pixels.iter().zip(&native.pixels).enumerate() {
                            if a.abs_diff(b) > 16 {
                                let (px, py) = ((i % ours.width) as i32, (i / ours.width) as i32);
                                (x0, y0, x1, y1) = (x0.min(px), y0.min(py), x1.max(px), y1.max(py));
                            }
                        }
                        misses.push(format!(
                            "glyph {glyph} at {size} px {scale}x x={x} #{color:06x}: differs in x {x0}..={x1} y {y0}..={y1}, our bitmap (left, top, w, h) {:?}",
                            ours.placed
                        ));
                    }
                }
            }
            eprintln!(
                "{size:>4} px at {scale}x ({:>2} ppem) w{weight} #{color:06x} | {}",
                size * scale,
                diff.row()
            );
            worst = worst.max(diff.max);
        }
    }
    for miss in &misses {
        eprintln!("  miss: {miss}");
    }
    assert!(
        worst <= 1,
        "a glyph mask misses CoreText's by {worst} codes"
    );
}

/// #102 end to end: a line laid out, rasterised and composited by Metal as a window draws it,
/// against CoreText's line mask blended `a F + (1 - a) dst` on encoded sRGB values.
#[test]
fn text_composites_like_a_native_label() {
    let Ok(mut renderer) = MetalRenderer::new_headless(Default::default()) else {
        eprintln!("skipped: no Metal device");
        return;
    };
    let ts = AppleTextSystem::new();
    let font_id = system_font_id(&ts, 400.);
    let text = "Hamburgefonstiv 0123";
    let mut worst = 0u8;
    eprintln!("\n#102 composited line {text:?} against CoreText's mask blended in encoded sRGB");
    for (size, scale) in [(13.0f32, 2.0f32), (24.0, 2.0), (13.0, 1.0), (24.0, 1.0)] {
        let native_font = native_system_font(size as f64, 400.);
        for (fg, bg) in [
            (0x000000u32, 0xffffffu32),
            (0xffffff, 0x000000),
            (0x808080, 0xffffff),
            (0xbfbfbf, 0x1e1e1e),
            (0xff0000, 0xffffff),
            (0x0a84ff, 0x1e1e1e),
        ] {
            let ppem = size * scale;
            let mut native = Canvas::for_size(ppem, text.len() as f32 * 0.62);
            native.draw_native_line(&native_font, text, scale as f64, srgb(fg));
            let (width, height) = (native.width, native.height);

            let mut scene = Scene::default();
            let frame = Bounds::new(
                gpui::point(ScaledPixels(0.), ScaledPixels(0.)),
                gpui::size(ScaledPixels(width as f32), ScaledPixels(height as f32)),
            );
            scene.insert_primitive(Quad {
                bounds: frame,
                background: rgb(bg).into(),
                content_mask: ContentMask::rect(frame),
                ..Default::default()
            });
            let level = ts.glyph_dilation_for_color(hsla(fg));
            for (font_id, glyph, x) in our_glyphs(&ts, font_id, text, size) {
                let origin = gpui::point(
                    ScaledPixels(native.x0 as f32 + x * scale),
                    ScaledPixels(native.y0 as f32),
                );
                let (whole, variant) = gpui::quantize_glyph_origin(origin, ppem);
                let mut params = params(font_id, glyph, size, scale, level);
                params.subpixel_variant = variant;
                let bounds = ts.glyph_raster_bounds(&params).unwrap();
                if bounds.size.width.0 == 0 {
                    continue;
                }
                let tile = renderer
                    .sprite_atlas()
                    .get_or_insert_with(AtlasKey::from(params.clone()), &mut || {
                        let (size, bytes) = ts.rasterize_glyph(&params, bounds)?;
                        Ok(Some((size, Cow::Owned(bytes))))
                    })
                    .unwrap()
                    .unwrap();
                scene.insert_primitive(MonochromeSprite {
                    order: 0,
                    raw_coverage: false.into(),
                    animation: gpui::SpriteAnimationId::NONE,
                    pad: 0,
                    bounds: Bounds::new(
                        whole + bounds.origin.map(|v| ScaledPixels(v.0 as f32)),
                        tile.bounds.size.map(|v| ScaledPixels(v.0 as f32)),
                    ),
                    content_mask: ContentMask::rect(frame),
                    color: hsla(fg),
                    tile,
                    transformation: TransformationMatrix::unit(),
                });
            }
            scene.finish();
            let image = renderer
                .render_scene_to_image(
                    &scene,
                    gpui::size(DevicePixels(width as i32), DevicePixels(height as i32)),
                )
                .unwrap();

            let (f, d) = (srgb(fg), srgb(bg));
            if let Some(dir) = std::env::var_os("SK_TEXT_ORACLE_PNG") {
                let name = format!("{size}px-{scale}x-{fg:06x}-on-{bg:06x}");
                let dir = std::path::Path::new(&dir);
                image.save(dir.join(format!("{name}-skathi.png"))).unwrap();
                let native_image =
                    image::RgbaImage::from_fn(width as u32, height as u32, |x, y| {
                        let a = native.pixels[y as usize * width + x as usize] as f64 / 255.0;
                        let c = |i: usize| ((a * f[i] + (1.0 - a) * d[i]) * 255.0).round() as u8;
                        image::Rgba([c(0), c(1), c(2), 255])
                    });
                native_image
                    .save(dir.join(format!("{name}-coretext.png")))
                    .unwrap();
            }
            let (mut sum, mut squares, mut count, mut max) = (0f64, 0f64, 0usize, 0u8);
            let (mut over, mut worst_at) = (0usize, (0, 0));
            for y in 0..height {
                for x in 0..width {
                    let a = native.pixels[y * width + x] as f64 / 255.0;
                    let pixel = image.get_pixel(x as u32, y as u32).0;
                    let mut touched = a > 0.0;
                    for c in 0..3 {
                        let expected = ((a * f[c] + (1.0 - a) * d[c]) * 255.0).round() as u8;
                        let error = pixel[c].abs_diff(expected);
                        touched |= pixel[c].abs_diff((d[c] * 255.0).round() as u8) > 1;
                        if error > max {
                            worst_at = (x, y);
                        }
                        max = max.max(error);
                        over += (c == 0 && error > 16) as usize;
                        if touched {
                            sum += error as f64;
                            squares += (error as f64 / 255.0).powi(2);
                            count += 1;
                        }
                    }
                }
            }
            eprintln!(
                "{size:>4} px at {scale}x #{fg:06x} on #{bg:06x} | mean {:5.2} | rms {:.4} | max {:3} | pixels over 16: {over} (worst at {worst_at:?})",
                sum / count.max(1) as f64,
                (squares / count.max(1) as f64).sqrt(),
                max
            );
            worst = worst.max(max);
        }
    }
    assert!(
        worst <= 1,
        "composited text misses the native blend by {worst} codes"
    );
}
