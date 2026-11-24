use anyhow::anyhow;
use collections::{HashMap, HashSet};
use core_foundation::{
    array::{CFArray, CFArrayRef},
    attributed_string::CFMutableAttributedString,
    base::{CFRange, CFType, TCFType},
    dictionary::{CFDictionary, CFDictionaryRef},
    number::CFNumber,
    string::{CFString, CFStringRef},
};
use core_graphics::{
    base::{CGFloat, CGGlyph, kCGImageAlphaPremultipliedLast},
    color_space::CGColorSpace,
    context::{CGContext, CGTextDrawingMode},
    geometry::{CGAffineTransform, CGPoint},
};
use core_text::{
    font::{CTFont, CTFontRef},
    font_collection::CTFontCollectionRef,
    font_descriptor::{
        CTFontDescriptor, CTFontDescriptorRef, TraitAccessors as _, kCTFontDefaultOrientation,
        kCTFontSlantTrait, kCTFontSymbolicTrait, kCTFontVariationAttribute, kCTFontWeightTrait,
        kCTFontWidthTrait,
    },
    line::CTLine,
    string_attributes::kCTFontAttributeName,
};
use font_kit::{
    font::Font as FontKitFont,
    handle::Handle,
    metrics::Metrics,
    properties::{Style as FontkitStyle, Weight as FontkitWeight},
    source::SystemSource,
    sources::mem::MemSource,
};
use gpui::{
    Bounds, DevicePixels, Font, FontFallbacks, FontFeatures, FontId, FontMetrics, FontRun,
    FontStyle, FontWeight, GlyphId, LineLayout, Pixels, PlatformTextSystem, RenderGlyphParams,
    Result, ShapedGlyph, ShapedRun, SharedString, Size, TextRenderingMode, point, px, size,
    swap_rgba_pa_to_bgra,
};
use parking_lot::{RwLock, RwLockUpgradableReadGuard};
use pathfinder_geometry::{
    rect::{RectF, RectI},
    transform2d::Transform2F,
    vector::Vector2F,
};
use smallvec::SmallVec;
use std::{borrow::Cow, char, convert::TryFrom, sync::Arc};

#[cfg(target_os = "macos")]
use {
    gpui::{Hsla, Rgba},
    std::sync::OnceLock,
};

use crate::open_type::apply_features_and_fallbacks;

#[allow(non_upper_case_globals)]
const kCGImageAlphaOnly: u32 = 7;

/// Apple text system using CoreText for font shaping.
pub struct AppleTextSystem(RwLock<AppleTextSystemState>);

#[derive(Clone, PartialEq, Eq, Hash)]
struct FontKey {
    font_family: SharedString,
    font_features: FontFeatures,
    font_fallbacks: Option<FontFallbacks>,
}

struct AppleTextSystemState {
    memory_source: MemSource,
    system_source: SystemSource,
    fonts: Vec<FontKitFont>,
    font_selections: HashMap<Font, FontId>,
    font_ids_by_postscript_name: HashMap<String, FontId>,
    font_ids_by_font_key: HashMap<FontKey, SmallVec<[FontId; 4]>>,
    postscript_names_by_font_id: HashMap<FontId, String>,
    /// vue-native fork: `font-family` lists and composed families, which Core Text's cascade
    /// cannot draw: it does not see fonts added from memory.
    lists: gpui::FontLists,
    /// vue-native fork: the fonts added from memory. A variable one is drawn at the weight
    /// asked for ([`AppleTextSystemState::at_weight`]); installed families keep Core Text's own
    /// instances.
    memory_fonts: HashSet<FontId>,
    /// Instances of a font at a `wght`, by the font and the coordinate's bits.
    weight_instances: HashMap<(FontId, u32), FontId>,
    /// The same instances by PostScript name, with their `wght` in hundredths, for the fonts of
    /// the runs Core Text shapes: an instance can share its name with another.
    instance_ids: HashMap<String, Vec<(i64, FontId)>>,
}

impl AppleTextSystem {
    /// Create a new AppleTextSystem.
    pub fn new() -> Self {
        Self(RwLock::new(AppleTextSystemState {
            memory_source: MemSource::empty(),
            system_source: SystemSource::new(),
            fonts: Vec::new(),
            font_selections: HashMap::default(),
            font_ids_by_postscript_name: HashMap::default(),
            font_ids_by_font_key: HashMap::default(),
            postscript_names_by_font_id: HashMap::default(),
            lists: gpui::FontLists::default(),
            memory_fonts: HashSet::default(),
            weight_instances: HashMap::default(),
            instance_ids: HashMap::default(),
        }))
    }
}

impl Default for AppleTextSystem {
    fn default() -> Self {
        Self::new()
    }
}

impl PlatformTextSystem for AppleTextSystem {
    fn add_fonts(&self, fonts: Vec<Cow<'static, [u8]>>) -> Result<()> {
        self.0.write().add_fonts(fonts)
    }

    fn all_font_names(&self) -> Vec<String> {
        let mut names = Vec::new();
        let collection = core_text::font_collection::create_for_all_families();
        // NOTE: We intentionally avoid using `collection.get_descriptors()` here because
        // it has a memory leak bug in core-text v21.0.0. The upstream code uses
        // `wrap_under_get_rule` but `CTFontCollectionCreateMatchingFontDescriptors`
        // follows the Create Rule (caller owns the result), so it should use
        // `wrap_under_create_rule`. We call the function directly with correct memory management.
        unsafe extern "C" {
            fn CTFontCollectionCreateMatchingFontDescriptors(
                collection: CTFontCollectionRef,
            ) -> CFArrayRef;
        }
        let descriptors: Option<CFArray<CTFontDescriptor>> = unsafe {
            let array_ref =
                CTFontCollectionCreateMatchingFontDescriptors(collection.as_concrete_TypeRef());
            if array_ref.is_null() {
                None
            } else {
                Some(CFArray::wrap_under_create_rule(array_ref))
            }
        };
        let Some(descriptors) = descriptors else {
            return names;
        };
        for descriptor in descriptors.into_iter() {
            names.extend(lenient_font_attributes::family_name(&descriptor));
        }
        if let Ok(fonts_in_memory) = self.0.read().memory_source.all_families() {
            names.extend(fonts_in_memory);
        }
        names
    }

    fn font_id(&self, font: &Font) -> Result<FontId> {
        let lock = self.0.upgradable_read();
        if let Some(font_id) = lock.font_selections.get(font) {
            Ok(*font_id)
        } else {
            let mut lock = RwLockUpgradableReadGuard::upgrade(lock);
            if lock.lists.takes(font) {
                return gpui::resolve_listed_font(&mut *lock, font)
                    .ok_or_else(|| anyhow!("no family of {:?} resolves", font.family));
            }
            lock.plain_font_id(font)
        }
    }

    fn set_composed_families(&self, families: Vec<(SharedString, gpui::FamilyComposer)>) {
        let mut state = self.0.write();
        state.lists.set_composed_families(families);
        let AppleTextSystemState {
            lists,
            font_selections,
            ..
        } = &mut *state;
        font_selections.retain(|font, _| !lists.takes(font));
    }

    fn font_metrics(&self, font_id: FontId) -> FontMetrics {
        font_kit_metrics_to_metrics(self.0.read().fonts[font_id.0].metrics())
    }

    fn typographic_bounds(&self, font_id: FontId, glyph_id: GlyphId) -> Result<Bounds<f32>> {
        Ok(bounds_from_rect(
            self.0.read().fonts[font_id.0].typographic_bounds(glyph_id.0)?,
        ))
    }

    fn advance(&self, font_id: FontId, glyph_id: GlyphId) -> Result<Size<f32>> {
        self.0.read().advance(font_id, glyph_id)
    }

    fn glyph_for_char(&self, font_id: FontId, ch: char) -> Option<GlyphId> {
        self.0.read().glyph_for_char(font_id, ch)
    }

    fn glyph_raster_bounds(&self, params: &RenderGlyphParams) -> Result<Bounds<DevicePixels>> {
        self.0.read().raster_bounds(params)
    }

    fn rasterize_glyph(
        &self,
        glyph_id: &RenderGlyphParams,
        raster_bounds: Bounds<DevicePixels>,
    ) -> Result<(Size<DevicePixels>, Vec<u8>)> {
        self.0.read().rasterize_glyph(glyph_id, raster_bounds)
    }

    fn layout_line(&self, text: &str, font_size: Pixels, font_runs: &[FontRun]) -> LineLayout {
        self.0.write().layout_line(text, font_size, font_runs)
    }

    fn recommended_rendering_mode(
        &self,
        _font_id: FontId,
        _font_size: Pixels,
    ) -> TextRenderingMode {
        TextRenderingMode::Grayscale
    }

    /// 0 draws without font smoothing; 1 to 5 are CoreGraphics' five dilation levels, chosen by
    /// the colour's linear luminance to the nearest quarter, as it chooses them itself.
    #[cfg(target_os = "macos")]
    fn glyph_dilation_for_color(&self, color: Hsla) -> u8 {
        if !font_smoothing_allowed_by_user() {
            return 0;
        }
        let rgba: Rgba = color.into();
        let decode = |u: f32| {
            if u <= 0.04045 {
                u / 12.92
            } else {
                ((u + 0.055) / 1.055).powf(2.4)
            }
        };
        let luminance = 0.2126 * decode(rgba.r) + 0.7152 * decode(rgba.g) + 0.0722 * decode(rgba.b);
        1 + (4.0 * luminance).round().clamp(0.0, 4.0) as u8
    }
}

/// The fill a glyph of a dilation level is drawn with: a grey whose linear luminance is the
/// level's, so CoreGraphics dilates it as it would the text colour.
#[cfg(target_os = "macos")]
fn dilation_fill(dilation: u8) -> core_graphics::color::CGColor {
    let linear = dilation.saturating_sub(1).min(4) as f64 / 4.0;
    let encoded = if linear <= 0.0031308 {
        linear * 12.92
    } else {
        1.055 * linear.powf(1.0 / 2.4) - 0.055
    };
    srgb_color(encoded, encoded, encoded)
}

#[cfg(target_os = "macos")]
fn srgb_color(red: f64, green: f64, blue: f64) -> core_graphics::color::CGColor {
    unsafe extern "C" {
        fn CGColorCreateSRGB(
            red: CGFloat,
            green: CGFloat,
            blue: CGFloat,
            alpha: CGFloat,
        ) -> core_graphics::color::SysCGColorRef;
    }
    // SAFETY: CGColorCreateSRGB returns a new colour the wrapper takes ownership of.
    unsafe {
        core_graphics::color::CGColor::wrap_under_create_rule(CGColorCreateSRGB(
            red, green, blue, 1.0,
        ))
    }
}

#[cfg(target_os = "macos")]
fn font_smoothing_allowed_by_user() -> bool {
    static ALLOWED: OnceLock<bool> = OnceLock::new();
    *ALLOWED.get_or_init(|| {
        use core_foundation_sys::preferences::{
            CFPreferencesCopyAppValue, kCFPreferencesCurrentApplication,
        };

        let key = CFString::new("AppleFontSmoothing");
        let value_ref = unsafe {
            CFPreferencesCopyAppValue(key.as_concrete_TypeRef(), kCFPreferencesCurrentApplication)
        };
        if value_ref.is_null() {
            return true;
        }
        let value = unsafe { CFType::wrap_under_create_rule(value_ref) };
        let Some(number) = value.downcast_into::<CFNumber>() else {
            return true;
        };
        // Only an explicit value of `0` means that font smoothing is disabled.
        number.to_i64() != Some(0)
    })
}

impl gpui::FontListHost for AppleTextSystemState {
    fn plain_font(&mut self, font: &Font) -> Option<FontId> {
        self.plain_font_id(font).ok()
    }

    fn alias_font(&mut self, of: FontId) -> FontId {
        let font_id = FontId(self.fonts.len());
        self.fonts.push(self.fonts[of.0].clone());
        if let Some(name) = self.postscript_names_by_font_id.get(&of).cloned() {
            self.postscript_names_by_font_id.insert(font_id, name);
        }
        font_id
    }

    fn font_lists(&mut self) -> &mut gpui::FontLists {
        &mut self.lists
    }
}

impl AppleTextSystemState {
    fn plain_font_id(&mut self, font: &Font) -> Result<FontId> {
        if let Some(font_id) = self.font_selections.get(font) {
            return Ok(*font_id);
        }
        let font_key = FontKey {
            font_family: font.family.clone(),
            font_features: font.features.clone(),
            font_fallbacks: font.fallbacks.clone(),
        };
        let candidates = if let Some(font_ids) = self.font_ids_by_font_key.get(&font_key) {
            font_ids.as_slice()
        } else {
            let font_ids =
                self.load_family(&font.family, &font.features, font.fallbacks.as_ref())?;
            self.font_ids_by_font_key.insert(font_key.clone(), font_ids);
            self.font_ids_by_font_key[&font_key].as_ref()
        };

        let candidate_properties = candidates
            .iter()
            .map(|font_id| {
                let font = &self.fonts[font_id.0];
                let mut properties = font.properties();
                properties.weight =
                    css_weight_of_trait(font.native_font().all_traits().normalized_weight());
                properties
            })
            .collect::<SmallVec<[_; 4]>>();

        let ix = font_kit::matching::find_best_match(
            &candidate_properties,
            &font_kit::properties::Properties {
                style: fontkit_style(font.style),
                weight: fontkit_weight(font.weight),
                stretch: Default::default(),
            },
        )?;

        let mut font_id = candidates[ix];
        if self.memory_fonts.contains(&font_id) {
            font_id = self.at_weight(font_id, font.weight);
        }
        self.font_selections.insert(font.clone(), font_id);
        Ok(font_id)
    }

    /// `font_id` instanced at `weight`, clamped to its `wght` axis; `font_id` itself when it has
    /// no such axis or is that instance already. Shaping, metrics and glyphs all come from the
    /// instance's CTFont.
    fn at_weight(&mut self, font_id: FontId, weight: FontWeight) -> FontId {
        let native = self.fonts[font_id.0].native_font();
        let Some(axis) = weight_axis(&native) else {
            return font_id;
        };
        let wght = (weight.0 as f64).clamp(axis.min, axis.max);
        if (variation_value(&native, WGHT).unwrap_or(axis.default) - wght).abs() < 0.01 {
            return font_id;
        }
        let key = (font_id, (wght as f32).to_bits());
        if let Some(&instance) = self.weight_instances.get(&key) {
            return instance;
        }
        let Some(instance) = with_variation(&native, WGHT, wght) else {
            return font_id;
        };
        let instance_id = FontId(self.fonts.len());
        // SAFETY: `instance` is a valid CTFont, retained by the font-kit font.
        self.fonts
            .push(unsafe { FontKitFont::from_native_font(&instance) });
        let name = instance.postscript_name();
        self.instance_ids
            .entry(name.clone())
            .or_default()
            .push((wght_key(wght), instance_id));
        self.postscript_names_by_font_id.insert(instance_id, name);
        self.weight_instances.insert(key, instance_id);
        instance_id
    }

    fn add_fonts(&mut self, fonts: Vec<Cow<'static, [u8]>>) -> Result<()> {
        self.lists.clear_resolved();
        let fonts = fonts
            .into_iter()
            .map(|bytes| match bytes {
                Cow::Borrowed(embedded_font) => {
                    let data_provider = unsafe {
                        core_graphics::data_provider::CGDataProvider::from_slice(embedded_font)
                    };
                    let font = core_graphics::font::CGFont::from_data_provider(data_provider)
                        .map_err(|()| anyhow!("Could not load an embedded font."))?;
                    let font = font_kit::loaders::core_text::Font::from_core_graphics_font(font);
                    Ok(Handle::from_native(&font))
                }
                Cow::Owned(bytes) => Ok(Handle::from_memory(Arc::new(bytes), 0)),
            })
            .collect::<Result<Vec<_>>>()?;
        self.memory_source.add_fonts(fonts.into_iter())?;
        Ok(())
    }

    fn load_family(
        &mut self,
        name: &str,
        features: &FontFeatures,
        fallbacks: Option<&FontFallbacks>,
    ) -> Result<SmallVec<[FontId; 4]>> {
        let name = gpui::font_name_with_fallbacks(name, ".AppleSystemUIFont");

        let mut font_ids = SmallVec::new();
        let mut postscript_names_seen = HashSet::default();
        let added = self.memory_source.select_family_by_name(name).ok();
        // vue-native fork: a font the app added (a web font's `unicode-range` subset) is asked
        // for by name and draws only the characters it has.
        let needs_m = added.is_none();
        let from_memory = added.is_some();
        let family = match added {
            Some(family) => family,
            None => self.system_source.select_family_by_name(name)?,
        };
        for font in family.fonts() {
            let mut font = font.load()?;

            apply_features_and_fallbacks(&mut font, features, fallbacks)?;
            // This block contains a precautionary fix to guard against loading fonts
            // that might cause panics due to `.unwrap()`s up the chain.
            {
                // We use the 'm' character for text measurements in various spots
                // (e.g., the editor). However, at time of writing some of those usages
                // will panic if the font has no 'm' glyph.
                //
                // Therefore, we check up front that the font has the necessary glyph.
                let has_m_glyph = font.glyph_for_char('m').is_some();

                // HACK: The 'Segoe Fluent Icons' font does not have an 'm' glyph,
                // but we need to be able to load it for rendering Windows icons in
                // the Storybook (on macOS).
                let is_segoe_fluent_icons = font.full_name() == "Segoe Fluent Icons";

                if needs_m && !has_m_glyph && !is_segoe_fluent_icons {
                    // I spent far too long trying to track down why a font missing the 'm'
                    // character wasn't loading. This log statement will hopefully save
                    // someone else from suffering the same fate.
                    log::warn!(
                        "font '{}' has no 'm' character and was not loaded",
                        font.full_name()
                    );
                    continue;
                }
            }

            // We've seen a number of panics in production caused by calling font.properties()
            // which unwraps a downcast to CFNumber. This is an attempt to avoid the panic,
            // and to try and identify the incalcitrant font.
            let traits = font.native_font().all_traits();
            if unsafe {
                !(traits
                    .get(kCTFontSymbolicTrait)
                    .downcast::<CFNumber>()
                    .is_some()
                    && traits
                        .get(kCTFontWidthTrait)
                        .downcast::<CFNumber>()
                        .is_some()
                    && traits
                        .get(kCTFontWeightTrait)
                        .downcast::<CFNumber>()
                        .is_some()
                    && traits
                        .get(kCTFontSlantTrait)
                        .downcast::<CFNumber>()
                        .is_some())
            } {
                log::error!(
                    "Failed to read traits for font {:?} (PostScript name {:?})",
                    font.full_name(),
                    font.postscript_name(),
                );
                continue;
            }

            let Some(postscript_name) = font.postscript_name() else {
                log::warn!(
                    "font {:?} in family {:?} has no PostScript name; skipping",
                    font.full_name(),
                    name,
                );
                continue;
            };
            // Dedup is scoped to this single `load_family` call (issue #55472).
            // The same family can be reloaded later under a different `FontKey`
            // (different features/fallbacks); a global check against
            // `font_ids_by_postscript_name` would skip every already-registered
            // font and leave the second call's `font_ids` empty.
            if !postscript_names_seen.insert(postscript_name.clone()) {
                log::warn!(
                    "skipping duplicate font {:?} with PostScript name {:?} \
                     in family {:?}",
                    font.full_name(),
                    postscript_name,
                    name,
                );
                continue;
            }
            let font_id = FontId(self.fonts.len());
            font_ids.push(font_id);
            if from_memory {
                self.memory_fonts.insert(font_id);
            }
            self.font_ids_by_postscript_name
                .insert(postscript_name.clone(), font_id);
            self.postscript_names_by_font_id
                .insert(font_id, postscript_name);
            self.fonts.push(font);
        }
        Ok(font_ids)
    }

    fn advance(&self, font_id: FontId, glyph_id: GlyphId) -> Result<Size<f32>> {
        Ok(size_from_vector2f(
            self.fonts[font_id.0].advance(glyph_id.0)?,
        ))
    }

    fn glyph_for_char(&self, font_id: FontId, ch: char) -> Option<GlyphId> {
        self.fonts[font_id.0].glyph_for_char(ch).map(GlyphId)
    }

    fn id_for_native_font(&mut self, requested_font: CTFont) -> FontId {
        let postscript_name = requested_font.postscript_name();
        if let Some(instances) = self.instance_ids.get(&postscript_name)
            && let Some(wght) = variation_value(&requested_font, WGHT)
            && let Some(&(_, font_id)) = instances.iter().find(|(at, _)| *at == wght_key(wght))
        {
            return font_id;
        }
        if let Some(font_id) = self.font_ids_by_postscript_name.get(&postscript_name) {
            *font_id
        } else {
            let font_id = FontId(self.fonts.len());
            self.font_ids_by_postscript_name
                .insert(postscript_name.clone(), font_id);
            self.postscript_names_by_font_id
                .insert(font_id, postscript_name);
            self.fonts
                .push(font_kit::font::Font::from_core_graphics_font(
                    requested_font.copy_to_CGFont(),
                ));
            font_id
        }
    }

    fn is_emoji(&self, font_id: FontId) -> bool {
        self.postscript_names_by_font_id
            .get(&font_id)
            .is_some_and(|postscript_name| {
                postscript_name == "AppleColorEmoji" || postscript_name == ".AppleColorEmojiUI"
            })
    }

    fn raster_bounds(&self, params: &RenderGlyphParams) -> Result<Bounds<DevicePixels>> {
        // The font at the size it is drawn: a variable system font changes its design with the
        // size (optical size), so bounds scaled from another size can miss a pixel column.
        let font = self.fonts[params.font_id.0]
            .native_font()
            .clone_with_font_size(f32::from(params.font_size) as CGFloat);
        let rect = font.get_bounding_rects_for_glyphs(
            kCTFontDefaultOrientation,
            &[params.glyph_id.0 as CGGlyph],
        );
        let rect = RectF::new(
            Vector2F::new(
                rect.origin.x as f32,
                -(rect.origin.y + rect.size.height) as f32,
            ),
            Vector2F::new(rect.size.width as f32, rect.size.height as f32),
        );
        let scale = Transform2F::from_scale(params.scale_factor);
        let bounds = bounds_from_rect_i((scale * rect).round_out().to_i32());

        // Expand the bounds by 1 pixel on each side to give CG room for anti-aliasing.
        Ok(bounds.dilate(DevicePixels(1)))
    }

    fn rasterize_glyph(
        &self,
        params: &RenderGlyphParams,
        glyph_bounds: Bounds<DevicePixels>,
    ) -> Result<(Size<DevicePixels>, Vec<u8>)> {
        if glyph_bounds.size.width.0 == 0 || glyph_bounds.size.height.0 == 0 {
            anyhow::bail!("glyph bounds are empty");
        } else {
            // Add an extra pixel when the subpixel variant isn't zero to make room for anti-aliasing.
            let mut bitmap_size = glyph_bounds.size;
            if params.subpixel_variant.x > 0 {
                bitmap_size.width += DevicePixels(1);
            }
            if params.subpixel_variant.y > 0 {
                bitmap_size.height += DevicePixels(1);
            }
            let bitmap_size = bitmap_size;

            let mut bytes;
            let cx;
            if params.is_emoji {
                bytes = vec![0; bitmap_size.width.0 as usize * 4 * bitmap_size.height.0 as usize];
                cx = CGContext::create_bitmap_context(
                    Some(bytes.as_mut_ptr() as *mut _),
                    bitmap_size.width.0 as usize,
                    bitmap_size.height.0 as usize,
                    8,
                    bitmap_size.width.0 as usize * 4,
                    &CGColorSpace::create_device_rgb(),
                    kCGImageAlphaPremultipliedLast,
                );
            } else {
                bytes = vec![0; bitmap_size.width.0 as usize * bitmap_size.height.0 as usize];
                cx = CGContext::create_bitmap_context(
                    Some(bytes.as_mut_ptr() as *mut _),
                    bitmap_size.width.0 as usize,
                    bitmap_size.height.0 as usize,
                    8,
                    bitmap_size.width.0 as usize,
                    &CGColorSpace::create_device_gray(),
                    kCGImageAlphaOnly,
                );
            }

            // Move the origin to bottom left and account for scaling, this
            // makes drawing text consistent with the font-kit's raster_bounds.
            cx.translate(
                -glyph_bounds.origin.x.0 as CGFloat,
                (glyph_bounds.origin.y.0 + glyph_bounds.size.height.0) as CGFloat,
            );
            cx.scale(
                params.scale_factor as CGFloat,
                params.scale_factor as CGFloat,
            );

            let subpixel_shift = gpui::subpixel_variant_offset(params.subpixel_variant);
            // The window picks the phase by CoreGraphics' own rule, so with quantisation on the
            // mask is the one a native label gets at that phase. Unquantised positions snap to
            // a 1/128 grid of their own and miss thirds by a code or two.
            let quantize = gpui::text_experiment().phases;
            cx.set_text_drawing_mode(CGTextDrawingMode::CGTextFill);
            cx.set_allows_antialiasing(true);
            cx.set_should_antialias(true);
            cx.set_allows_font_subpixel_positioning(true);
            cx.set_should_subpixel_position_fonts(true);
            cx.set_allows_font_subpixel_quantization(quantize);
            cx.set_should_subpixel_quantize_fonts(quantize);

            #[cfg(target_os = "macos")]
            if !params.is_emoji {
                cx.set_allows_font_smoothing(true);
                cx.set_should_smooth_fonts(params.dilation > 0);
                cx.set_fill_color(&dilation_fill(params.dilation));
            } else {
                cx.set_gray_fill_color(0.0, 1.0);
            }
            #[cfg(not(target_os = "macos"))]
            cx.set_gray_fill_color(0.0, 1.0);
            self.fonts[params.font_id.0]
                .native_font()
                .clone_with_font_size(f32::from(params.font_size) as CGFloat)
                .draw_glyphs(
                    &[params.glyph_id.0 as CGGlyph],
                    &[CGPoint::new(
                        (subpixel_shift.x / params.scale_factor) as CGFloat,
                        (subpixel_shift.y / params.scale_factor) as CGFloat,
                    )],
                    cx,
                );

            if params.is_emoji {
                // Convert from RGBA with premultiplied alpha to BGRA with straight alpha.
                for pixel in bytes.chunks_exact_mut(4) {
                    swap_rgba_pa_to_bgra(pixel);
                }
            }

            Ok((bitmap_size, bytes))
        }
    }

    fn layout_line(&mut self, text: &str, font_size: Pixels, font_runs: &[FontRun]) -> LineLayout {
        let fonts = &self.fonts;
        let split = self.lists.split_runs(text, font_runs, |font_id, c| {
            fonts[font_id.0].glyph_for_char(c).is_some()
        });
        let font_runs: &[FontRun] = &split;
        // Construct the attributed string, converting UTF8 ranges to UTF16 ranges.
        let mut string = CFMutableAttributedString::new();
        let mut max_ascent = 0.0f32;
        let mut max_descent = 0.0f32;

        {
            let mut text = text;
            // CoreText merges adjacent runs that end up in equal fonts, including runs of
            // different fonts that fall back to the same one, so every other run that inserts
            // text is set a hair larger to keep ligatures from forming across them. The first
            // run keeps its exact size: SF tracks by its exact point size, and a nudged size
            // draws wider or narrower than AppKit sets the same text.
            let mut break_ligature = false;
            for run in font_runs {
                let text_run;
                (text_run, text) = text.split_at(run.len);

                let utf16_start = string.char_len(); // insert at end of string
                // note: replace_str may silently ignore codepoints it dislikes (e.g., BOM at start of string)
                string.replace_str(&CFString::new(text_run), CFRange::init(utf16_start, 0));
                let utf16_end = string.char_len();

                let length = utf16_end - utf16_start;
                let cf_range = CFRange::init(utf16_start, length);
                let font = &self.fonts[run.font_id.0];

                let font_metrics = font.metrics();
                let font_scale = f32::from(font_size) / font_metrics.units_per_em as f32;
                max_ascent = max_ascent.max(font_metrics.ascent * font_scale);
                max_descent = max_descent.max(-font_metrics.descent * font_scale);

                let font_size = if break_ligature {
                    px(f32::from(font_size).next_up())
                } else {
                    font_size
                };
                unsafe {
                    string.set_attribute(
                        cf_range,
                        kCTFontAttributeName,
                        &font.native_font().clone_with_font_size(font_size.into()),
                    );
                }
                if length > 0 {
                    break_ligature = !break_ligature;
                }
            }
        }
        // Retrieve the glyphs from the shaped line, converting UTF16 offsets to UTF8 offsets.
        let line = CTLine::new_with_attributed_string(string.as_concrete_TypeRef());
        let glyph_runs = line.glyph_runs();
        let mut runs = <Vec<ShapedRun>>::with_capacity(glyph_runs.len() as usize);
        let mut ix_converter = StringIndexConverter::new(text);
        for run in glyph_runs.into_iter() {
            let attributes = run.attributes().unwrap();
            let font = unsafe {
                attributes
                    .get(kCTFontAttributeName)
                    .downcast::<CTFont>()
                    .unwrap()
            };
            let font_id = self.id_for_native_font(font);

            let glyphs = match runs.last_mut() {
                Some(run) if run.font_id == font_id => &mut run.glyphs,
                _ => {
                    runs.push(ShapedRun::new(
                        font_id,
                        Vec::with_capacity(run.glyph_count().try_into().unwrap_or(0)),
                    ));
                    &mut runs.last_mut().unwrap().glyphs
                }
            };
            for ((&glyph_id, position), &glyph_utf16_ix) in run
                .glyphs()
                .iter()
                .zip(run.positions().iter())
                .zip(run.string_indices().iter())
            {
                let glyph_utf16_ix = usize::try_from(glyph_utf16_ix).unwrap();
                if ix_converter.utf16_ix > glyph_utf16_ix {
                    // We cannot reuse current index converter, as it can only seek forward. Restart the search.
                    ix_converter = StringIndexConverter::new(text);
                }
                ix_converter.advance_to_utf16_ix(glyph_utf16_ix);
                glyphs.push(ShapedGlyph {
                    id: GlyphId(glyph_id as u32),
                    position: point(position.x as f32, position.y as f32).map(px),
                    index: ix_converter.utf8_ix,
                    is_emoji: self.is_emoji(font_id),
                });
            }
        }
        let typographic_bounds = line.get_typographic_bounds();
        let notdef_shift = self.draw_last_resort_as_notdef(&mut runs, font_runs, font_size);
        LineLayout {
            runs,
            font_size,
            width: px(typographic_bounds.width as f32 + notdef_shift),
            ascent: max_ascent.into(),
            descent: max_descent.into(),
            len: text.len(),
        }
    }
}

impl AppleTextSystemState {
    /// vue-native fork: Core Text draws a character no font has with LastResort's symbol for its
    /// block; DirectWrite and cosmic-text draw the first font's glyph 0 (.notdef), as browsers
    /// do. Each LastResort glyph becomes glyph 0 of the font its run asked for, and what follows
    /// moves by the difference of their advances. Returns how much wider the line became.
    fn draw_last_resort_as_notdef(
        &self,
        runs: &mut Vec<ShapedRun>,
        font_runs: &[FontRun],
        font_size: Pixels,
    ) -> f32 {
        let is_last_resort = |state: &Self, font_id: FontId| {
            state
                .postscript_names_by_font_id
                .get(&font_id)
                .is_some_and(|name| name == "LastResort")
        };
        if !runs.iter().any(|run| is_last_resort(self, run.font_id)) {
            return 0.;
        }
        let advance = |state: &Self, font_id: FontId, glyph_id: GlyphId| {
            let font = &state.fonts[font_id.0];
            let units = font.advance(glyph_id.0).map_or(0., |advance| advance.x());
            units * f32::from(font_size) / font.metrics().units_per_em as f32
        };
        let asked_at = |index: usize| {
            let mut end = 0;
            font_runs
                .iter()
                .find(|run| {
                    end += run.len;
                    index < end
                })
                .or(font_runs.last())
                .map(|run| run.font_id)
        };

        // (x, growth) of each glyph replaced, so a glyph moves by the growth of those left of it.
        let mut replaced = Vec::new();
        let mut out: Vec<ShapedRun> = Vec::with_capacity(runs.len());
        for run in runs.drain(..) {
            let last_resort = is_last_resort(self, run.font_id);
            for mut glyph in run.glyphs {
                let mut font_id = run.font_id;
                if last_resort && let Some(asked) = asked_at(glyph.index) {
                    let growth =
                        advance(self, asked, GlyphId(0)) - advance(self, font_id, glyph.id);
                    replaced.push((glyph.position.x, growth));
                    glyph.id = GlyphId(0);
                    glyph.is_emoji = false;
                    font_id = asked;
                }
                match out.last_mut() {
                    Some(last) if last.font_id == font_id => last.glyphs.push(glyph),
                    _ => out.push(ShapedRun::new(font_id, vec![glyph])),
                }
            }
        }
        for glyph in out.iter_mut().flat_map(|run| run.glyphs.iter_mut()) {
            let x = glyph.position.x;
            let shift: f32 = replaced
                .iter()
                .filter(|(at, _)| *at < x)
                .map(|(_, growth)| growth)
                .sum();
            glyph.position.x += px(shift);
        }
        *runs = out;
        replaced.iter().map(|(_, growth)| growth).sum()
    }
}

#[derive(Debug, Clone)]
struct StringIndexConverter<'a> {
    text: &'a str,
    /// Index in UTF-8 bytes
    utf8_ix: usize,
    /// Index in UTF-16 code units
    utf16_ix: usize,
}

impl<'a> StringIndexConverter<'a> {
    fn new(text: &'a str) -> Self {
        Self {
            text,
            utf8_ix: 0,
            utf16_ix: 0,
        }
    }

    fn advance_to_utf16_ix(&mut self, utf16_target: usize) {
        for (ix, c) in self.text[self.utf8_ix..].char_indices() {
            if self.utf16_ix >= utf16_target {
                self.utf8_ix += ix;
                return;
            }
            self.utf16_ix += c.len_utf16();
        }
        self.utf8_ix = self.text.len();
    }
}

fn font_kit_metrics_to_metrics(metrics: Metrics) -> FontMetrics {
    FontMetrics {
        units_per_em: metrics.units_per_em,
        ascent: metrics.ascent,
        descent: metrics.descent,
        line_gap: metrics.line_gap,
        underline_position: metrics.underline_position,
        underline_thickness: metrics.underline_thickness,
        cap_height: metrics.cap_height,
        x_height: metrics.x_height,
        bounding_box: bounds_from_rect(metrics.bounding_box),
    }
}

fn bounds_from_rect(rect: RectF) -> Bounds<f32> {
    Bounds {
        origin: point(rect.origin_x(), rect.origin_y()),
        size: size(rect.width(), rect.height()),
    }
}

fn bounds_from_rect_i(rect: RectI) -> Bounds<DevicePixels> {
    Bounds {
        origin: point(DevicePixels(rect.origin_x()), DevicePixels(rect.origin_y())),
        size: size(DevicePixels(rect.width()), DevicePixels(rect.height())),
    }
}

// impl From<Vector2I> for Size<DevicePixels> {
//     fn from(value: Vector2I) -> Self {
//         size(value.x().into(), value.y().into())
//     }
// }

// impl From<RectI> for Bounds<i32> {
//     fn from(rect: RectI) -> Self {
//         Bounds {
//             origin: point(rect.origin_x(), rect.origin_y()),
//             size: size(rect.width(), rect.height()),
//         }
//     }
// }

// impl From<Point<u32>> for Vector2I {
//     fn from(size: Point<u32>) -> Self {
//         Vector2I::new(size.x as i32, size.y as i32)
//     }
// }

fn size_from_vector2f(vec: Vector2F) -> Size<f32> {
    size(vec.x(), vec.y())
}

const WGHT: i64 = 0x77676874;

unsafe extern "C" {
    static kCTFontVariationAxisIdentifierKey: CFStringRef;
    static kCTFontVariationAxisMinimumValueKey: CFStringRef;
    static kCTFontVariationAxisMaximumValueKey: CFStringRef;
    static kCTFontVariationAxisDefaultValueKey: CFStringRef;
    fn CTFontCopyVariation(font: CTFontRef) -> CFDictionaryRef;
    fn CTFontCreateCopyWithAttributes(
        font: CTFontRef,
        size: CGFloat,
        matrix: *const CGAffineTransform,
        attributes: CTFontDescriptorRef,
    ) -> CTFontRef;
}

struct VariationAxis {
    min: f64,
    max: f64,
    default: f64,
}

/// The font's `wght` axis, when it spans more than one weight.
fn weight_axis(font: &CTFont) -> Option<VariationAxis> {
    let number = |axis: &CFDictionary<CFString, CFType>, key: CFStringRef| {
        axis.find(key)?.downcast::<CFNumber>()?.to_f64()
    };
    // SAFETY: CoreText's constant keys.
    let (identifier, min, max, default) = unsafe {
        (
            kCTFontVariationAxisIdentifierKey,
            kCTFontVariationAxisMinimumValueKey,
            kCTFontVariationAxisMaximumValueKey,
            kCTFontVariationAxisDefaultValueKey,
        )
    };
    font.get_variation_axes()?.iter().find_map(|axis| {
        if number(&axis, identifier)? as i64 != WGHT {
            return None;
        }
        let axis = VariationAxis {
            min: number(&axis, min)?,
            max: number(&axis, max)?,
            default: number(&axis, default)?,
        };
        (axis.min < axis.max).then_some(axis)
    })
}

/// The font's coordinate on `axis`, when it states one.
fn variation_value(font: &CTFont, axis: i64) -> Option<f64> {
    // SAFETY: CTFontCopyVariation follows the create rule; null when the font has no variations.
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

/// `font` with `axis` at `value`, at the same size; `None` when Core Text does not take it.
fn with_variation(font: &CTFont, axis: i64, value: f64) -> Option<CTFont> {
    let variation =
        CFDictionary::from_CFType_pairs(&[(CFNumber::from(axis), CFNumber::from(value))]);
    // SAFETY: a CoreText constant.
    let key = unsafe { CFString::wrap_under_get_rule(kCTFontVariationAttribute) };
    let attributes = CFDictionary::from_CFType_pairs(&[(key, variation.as_CFType())]);
    let descriptor = core_text::font_descriptor::new_from_attributes(&attributes);
    // SAFETY: CTFontCreateCopyWithAttributes follows the create rule; size 0 keeps the size.
    let instance = unsafe {
        let font = CTFontCreateCopyWithAttributes(
            font.as_concrete_TypeRef(),
            0.0,
            std::ptr::null(),
            descriptor.as_concrete_TypeRef(),
        );
        (!font.is_null()).then(|| CTFont::wrap_under_create_rule(font))?
    };
    variation_value(&instance, axis)
        .is_some_and(|got| (got - value).abs() < 0.01)
        .then_some(instance)
}

fn wght_key(wght: f64) -> i64 {
    (wght * 100.0).round() as i64
}

/// A CoreText weight trait on the CSS scale, through AppKit's `NSFontWeight` constants (thin
/// -0.6 … medium 0.23, semibold 0.3, bold 0.4 … black 0.62). font-kit's own table puts medium
/// above 500, so `font-weight: 500` matched regular.
fn css_weight_of_trait(weight: f64) -> FontkitWeight {
    const STOPS: [(f64, f32); 9] = [
        (-0.8, 100.),
        (-0.6, 200.),
        (-0.4, 300.),
        (0.0, 400.),
        (0.23, 500.),
        (0.3, 600.),
        (0.4, 700.),
        (0.56, 800.),
        (0.62, 900.),
    ];
    let (first, last) = (STOPS[0], STOPS[STOPS.len() - 1]);
    if weight <= first.0 {
        return FontkitWeight(first.1);
    }
    for pair in STOPS.windows(2) {
        let ((t0, w0), (t1, w1)) = (pair[0], pair[1]);
        if weight <= t1 {
            let t = ((weight - t0) / (t1 - t0)) as f32;
            return FontkitWeight(w0 + t * (w1 - w0));
        }
    }
    FontkitWeight(last.1)
}

fn fontkit_weight(value: FontWeight) -> FontkitWeight {
    FontkitWeight(value.0)
}

fn fontkit_style(style: FontStyle) -> FontkitStyle {
    match style {
        FontStyle::Normal => FontkitStyle::Normal,
        FontStyle::Italic => FontkitStyle::Italic,
        FontStyle::Oblique => FontkitStyle::Oblique,
    }
}

// Some fonts may have no attributes despite `core_text` requiring them (and panicking).
// This is the same version as `core_text` has without `expect` calls.
mod lenient_font_attributes {
    use core_foundation::{
        base::{CFRetain, CFType, TCFType},
        string::{CFString, CFStringRef},
    };
    use core_text::font_descriptor::{
        CTFontDescriptor, CTFontDescriptorCopyAttribute, kCTFontFamilyNameAttribute,
    };

    pub fn family_name(descriptor: &CTFontDescriptor) -> Option<String> {
        unsafe { get_string_attribute(descriptor, kCTFontFamilyNameAttribute) }
    }

    fn get_string_attribute(
        descriptor: &CTFontDescriptor,
        attribute: CFStringRef,
    ) -> Option<String> {
        unsafe {
            let value = CTFontDescriptorCopyAttribute(descriptor.as_concrete_TypeRef(), attribute);
            if value.is_null() {
                return None;
            }

            let value = CFType::wrap_under_create_rule(value);
            assert!(value.instance_of::<CFString>());
            let s = wrap_under_get_rule(value.as_CFTypeRef() as CFStringRef);
            Some(s.to_string())
        }
    }

    unsafe fn wrap_under_get_rule(reference: CFStringRef) -> CFString {
        unsafe {
            assert!(!reference.is_null(), "Attempted to create a NULL object.");
            let reference = CFRetain(reference as *const ::std::os::raw::c_void) as CFStringRef;
            TCFType::wrap_under_create_rule(reference)
        }
    }
}

#[cfg(all(test, target_os = "macos"))]
mod coretext_oracle;

#[cfg(test)]
mod tests {
    use crate::AppleTextSystem;
    use gpui::{
        FontFallbacks, FontFeatures, FontId, FontRun, GlyphId, PlatformTextSystem, font, px,
    };
    use std::sync::Arc;

    #[test]
    fn test_reload_family_with_features_and_fallbacks() {
        let fonts = AppleTextSystem::new();
        let mut font = font("Helvetica");
        let original_id = fonts.font_id(&font).unwrap();
        font.features = FontFeatures::disable_ligatures();
        font.fallbacks = Some(FontFallbacks(Arc::new(vec!["Times".into()])));
        let configured_id = fonts.font_id(&font).unwrap();
        assert_ne!(original_id, configured_id);
        assert_eq!(fonts.font_id(&font).unwrap(), configured_id);
        assert!(fonts.glyph_for_char(configured_id, 'm').is_some());
        let layout = fonts.layout_line(
            "office",
            px(16.),
            &[FontRun {
                font_id: configured_id,
                len: 6,
                tracking: px(0.),
                word_spacing: px(0.),
            }],
        );
        assert_eq!(layout.len, 6);
        assert!(!layout.runs.is_empty());
    }

    #[test]
    fn test_layout_line_sets_a_lone_run_at_its_exact_size() {
        use core_foundation::attributed_string::CFMutableAttributedString;
        use core_foundation::base::{CFRange, TCFType};
        use core_foundation::string::CFString;
        use core_text::line::CTLine;
        use core_text::string_attributes::kCTFontAttributeName;

        let fonts = AppleTextSystem::new();
        let font_id = fonts.font_id(&font(".SystemUIFont")).unwrap();
        let text = "Hamburgefonstiv 0123 Wafer";
        let bom = "\u{feff}";
        let with_bom = format!("{bom}{text}");
        for size in [11., 17., 22.] {
            let exact = fonts.0.read().fonts[font_id.0]
                .native_font()
                .clone_with_font_size(size as f64);
            let mut string = CFMutableAttributedString::new();
            string.replace_str(&CFString::new(text), CFRange::init(0, 0));
            let range = CFRange::init(0, string.char_len());
            unsafe { string.set_attribute(range, kCTFontAttributeName, &exact) };
            let line = CTLine::new_with_attributed_string(string.as_concrete_TypeRef());
            let width = line.get_typographic_bounds().width as f32;

            let layout = fonts.layout_line(
                text,
                px(size),
                &[FontRun {
                    font_id,
                    len: text.len(),
                    tracking: px(0.),
                    word_spacing: px(0.),
                }],
            );
            assert_eq!(f32::from(layout.width), width, "{size} px");

            // replace_str drops a leading BOM, so its run must not count as the previous run.
            let layout = fonts.layout_line(
                &with_bom,
                px(size),
                &[
                    FontRun {
                        font_id,
                        len: bom.len(),
                        tracking: px(0.),
                        word_spacing: px(0.),
                    },
                    FontRun {
                        font_id,
                        len: text.len(),
                        tracking: px(0.),
                        word_spacing: px(0.),
                    },
                ],
            );
            assert_eq!(f32::from(layout.width), width, "{size} px after a BOM run");
        }
    }

    #[test]
    fn test_layout_line_breaks_ligatures_between_runs() {
        let fonts = AppleTextSystem::new();
        fonts
            .add_fonts(vec![
                include_bytes!("../../../assets/fonts/lilex/Lilex-Regular.ttf")
                    .as_slice()
                    .into(),
            ])
            .unwrap();
        let glyphs = |text: &str, runs: &[(FontId, usize)]| -> Vec<GlyphId> {
            let runs = runs
                .iter()
                .map(|&(font_id, len)| FontRun {
                    font_id,
                    len,
                    tracking: px(0.),
                    word_spacing: px(0.),
                })
                .collect::<Vec<_>>();
            fonts
                .layout_line(text, px(16.), &runs)
                .runs
                .iter()
                .flat_map(|run| run.glyphs.iter().map(|glyph| glyph.id))
                .collect()
        };

        let lilex = fonts.font_id(&font("Lilex")).unwrap();
        let gt = glyphs(">", &[(lilex, 1)])[0];
        assert_ne!(glyphs(">>>", &[(lilex, 3)]), vec![gt; 3], ">>> as one run");
        assert_eq!(
            glyphs(">>>", &[(lilex, 1), (lilex, 1), (lilex, 1)]),
            vec![gt; 3]
        );
        let mut split = glyphs(">>", &[(lilex, 2)]);
        split.push(gt);
        assert_eq!(glyphs(">>>", &[(lilex, 2), (lilex, 1)]), split);
        let mut split = vec![gt];
        split.extend(glyphs(">>", &[(lilex, 2)]));
        assert_eq!(glyphs(">>>", &[(lilex, 1), (lilex, 2)]), split);

        // Distinct font ids that resolve to the same font.
        let zed_mono = fonts.font_id(&font(".ZedMono")).unwrap();
        let mut no_fallbacks = font("Lilex");
        no_fallbacks.fallbacks = Some(FontFallbacks(Arc::new(Vec::new())));
        let no_fallbacks = fonts.font_id(&no_fallbacks).unwrap();
        for other in [zed_mono, no_fallbacks] {
            assert_ne!(lilex, other);
            assert_eq!(
                glyphs(">>>", &[(lilex, 1), (other, 1), (lilex, 1)]),
                vec![gt; 3]
            );
        }

        // Empty runs between two runs of one font.
        let times = fonts.font_id(&font("Times")).unwrap();
        let helvetica = fonts.font_id(&font("Helvetica")).unwrap();
        let system = fonts.font_id(&font(".SystemUIFont")).unwrap();
        let mut split = glyphs("f", &[(times, 1)]);
        split.extend(glyphs("i", &[(times, 1)]));
        assert_ne!(glyphs("fi", &[(times, 2)]), split, "fi as one run");
        assert_eq!(
            glyphs("fi", &[(times, 1), (helvetica, 0), (system, 0), (times, 1)]),
            split
        );

        // Runs of different fonts that both fall back to Geeza Pro for Arabic.
        assert_eq!(glyphs("لا", &[(helvetica, 4)]).len(), 1, "لا as one run");
        assert_eq!(glyphs("لا", &[(helvetica, 2), (times, 2)]).len(), 2);
    }

    fn lilex_lists() -> AppleTextSystem {
        let fonts = AppleTextSystem::new();
        fonts
            .add_fonts(vec![
                include_bytes!("../../../assets/fonts/lilex/Lilex-Regular.ttf")
                    .to_vec()
                    .into(),
            ])
            .unwrap();
        let digits: gpui::FamilyComposer = Arc::new(|_, _| {
            vec![gpui::ComposedFace {
                family: "Lilex".into(),
                ranges: Arc::from([(0x30, 0x39)]),
                weight: None,
            }]
        });
        let wide: gpui::FamilyComposer = Arc::new(|_, _| {
            vec![gpui::ComposedFace {
                family: "Lilex".into(),
                ranges: Arc::from([]),
                weight: None,
            }]
        });
        fonts.set_composed_families(vec![("VN Digits".into(), digits), ("VN Wide".into(), wide)]);
        fonts
    }

    fn listed(family: &str, fallbacks: &[&str]) -> gpui::Font {
        let mut listed = font(family);
        listed.fallbacks = Some(FontFallbacks::from_fonts(
            fallbacks.iter().map(|f| f.to_string()).collect(),
        ));
        listed
    }

    /// Each glyph's character, font, glyph and x.
    fn drawn_with(
        fonts: &AppleTextSystem,
        text: &str,
        font: &gpui::Font,
    ) -> Vec<(char, FontId, GlyphId, f32)> {
        let font_id = fonts.font_id(font).unwrap();
        let layout = fonts.layout_line(
            text,
            px(16.),
            &[FontRun {
                font_id,
                len: text.len(),
                tracking: px(0.),
                word_spacing: px(0.),
            }],
        );
        layout
            .runs
            .iter()
            .flat_map(|run| {
                run.glyphs.iter().map(|glyph| {
                    (
                        text[glyph.index..].chars().next().unwrap(),
                        run.font_id,
                        glyph.id,
                        f32::from(glyph.position.x),
                    )
                })
            })
            .collect()
    }

    #[test]
    fn test_each_character_takes_the_first_listed_family_that_covers_it() {
        let fonts = lilex_lists();
        let helvetica = fonts.font_id(&font("Helvetica")).unwrap();
        let lilex = fonts.font_id(&font("Lilex")).unwrap();
        let drawn = drawn_with(&fonts, "a1", &listed("VN Digits", &["Helvetica"]));
        let drawn_fonts: Vec<_> = drawn.iter().map(|d| (d.0, d.1)).collect();
        assert_eq!(drawn_fonts, vec![('a', helvetica), ('1', lilex)]);

        // A character the face's range covers but that it has no glyph for goes to the next
        // family.
        let lilex_lacks = ('\u{a0}'..'\u{3000}')
            .find(|&c| {
                fonts.glyph_for_char(lilex, c).is_none()
                    && fonts.glyph_for_char(helvetica, c).is_some()
            })
            .expect("a character Helvetica has and Lilex lacks");
        let text = format!("a{lilex_lacks}");
        let drawn = drawn_with(&fonts, &text, &listed("VN Wide", &["Helvetica"]));
        let drawn_fonts: Vec<_> = drawn.iter().map(|d| (d.0, d.1)).collect();
        assert_eq!(drawn_fonts, vec![('a', lilex), (lilex_lacks, helvetica)]);
    }

    #[test]
    fn test_a_character_no_font_has_is_the_first_familys_notdef() {
        let fonts = lilex_lists();
        let helvetica = fonts.font_id(&font("Helvetica")).unwrap();
        let notdef = {
            let state = fonts.0.read();
            let font = &state.fonts[helvetica.0];
            font.advance(0).unwrap().x() * 16. / font.metrics().units_per_em as f32
        };
        for asked in [font("Helvetica"), listed("Helvetica", &["VN Wide"])] {
            let drawn = drawn_with(&fonts, "a\u{10FFFD}b", &asked);
            assert_eq!(drawn.len(), 3);
            assert_eq!(drawn[0].1, helvetica);
            assert_eq!(
                (drawn[1].0, drawn[1].1, drawn[1].2),
                ('\u{10FFFD}', helvetica, GlyphId(0))
            );
            assert!(
                (drawn[2].3 - drawn[1].3 - notdef).abs() < 0.01,
                "{drawn:?}, .notdef {notdef}"
            );
        }
    }

    /// A variable font added from memory is shaped, measured and drawn at the weight asked for:
    /// the same line and glyph masks as Core Text gives the font with `wght` set explicitly.
    #[test]
    fn test_a_variable_font_is_drawn_at_the_weight_asked_for() {
        use super::{WGHT, variation_value};
        use core_foundation::{
            attributed_string::CFMutableAttributedString,
            base::{CFRange, TCFType},
            number::CFNumber,
            string::CFString,
        };
        use core_text::font_descriptor::CTFontDescriptorCreateCopyWithVariation;
        use core_text::{line::CTLine, string_attributes::kCTFontAttributeName};
        use font_kit::font::Font as FontKitFont;
        use gpui::{FontWeight, RenderGlyphParams};

        const MANROPE: &[u8] =
            include_bytes!("../../../assets/fonts/manrope/Manrope-latin-wght.ttf");
        const TEXT: &str = "Hamburgefonstiv";
        const SIZE: f32 = 20.0;
        let weights = [200.0f32, 300.0, 400.0, 450.0, 600.0, 800.0];

        let fonts = AppleTextSystem::new();
        fonts.add_fonts(vec![MANROPE.to_vec().into()]).unwrap();
        let family = fonts
            .0
            .read()
            .memory_source
            .all_families()
            .unwrap()
            .into_iter()
            .find(|name| name.starts_with("Manrope"))
            .unwrap();
        let params = |font_id, glyph, scale: f32| RenderGlyphParams {
            font_id,
            glyph_id: glyph,
            font_size: px(SIZE),
            subpixel_variant: Default::default(),
            scale_factor: scale,
            is_emoji: false,
            subpixel_rendering: false,
            dilation: 0,
        };
        let mask = |fonts: &AppleTextSystem, params: &RenderGlyphParams| {
            let bounds = fonts.glyph_raster_bounds(params).unwrap();
            if bounds.size.width.0 == 0 {
                return (bounds, Vec::new());
            }
            (bounds, fonts.rasterize_glyph(params, bounds).unwrap().1)
        };

        let mut rows = Vec::new();
        for weight in weights {
            let mut asked = font(family.clone());
            asked.weight = FontWeight(weight);
            let font_id = fonts.font_id(&asked).unwrap();
            let layout = fonts.layout_line(
                TEXT,
                px(SIZE),
                &[FontRun {
                    font_id,
                    len: TEXT.len(),
                    tracking: px(0.),
                    word_spacing: px(0.),
                }],
            );

            let explicit = {
                let data = core_text::font_manager::create_font_descriptor(MANROPE).unwrap();
                let descriptor = unsafe {
                    core_text::font_descriptor::CTFontDescriptor::wrap_under_create_rule(
                        CTFontDescriptorCreateCopyWithVariation(
                            data.as_concrete_TypeRef(),
                            CFNumber::from(WGHT).as_concrete_TypeRef(),
                            weight as f64,
                        ),
                    )
                };
                core_text::font::new_from_descriptor(&descriptor, SIZE as f64)
            };
            let reference = AppleTextSystem::new();
            reference
                .0
                .write()
                .fonts
                .push(unsafe { FontKitFont::from_native_font(&explicit) });
            let reference_width = {
                let mut string = CFMutableAttributedString::new();
                string.replace_str(&CFString::new(TEXT), CFRange::init(0, 0));
                let range = CFRange::init(0, string.char_len());
                unsafe { string.set_attribute(range, kCTFontAttributeName, &explicit) };
                CTLine::new_with_attributed_string(string.as_concrete_TypeRef())
                    .get_typographic_bounds()
                    .width as f32
            };

            let (mut ink, mut reference_ink, mut mismatched) = (0u64, 0u64, 0);
            let mut drawn_wght = None;
            for run in &layout.runs {
                drawn_wght =
                    variation_value(&fonts.0.read().fonts[run.font_id.0].native_font(), WGHT);
                for glyph in &run.glyphs {
                    for scale in [1.0, 2.0] {
                        let ours = mask(&fonts, &params(run.font_id, glyph.id, scale));
                        let theirs = mask(&reference, &params(FontId(0), glyph.id, scale));
                        ink += ours.1.iter().map(|&v| v as u64).sum::<u64>();
                        reference_ink += theirs.1.iter().map(|&v| v as u64).sum::<u64>();
                        mismatched += (ours != theirs) as usize;
                    }
                }
            }
            eprintln!(
                "wght {weight:>4}: drawn at {drawn_wght:?} | width {:8.3} (Core Text, wght set: {reference_width:8.3}) | ink {ink:7} ({reference_ink:7}) | masks off {mismatched}",
                f32::from(layout.width)
            );
            rows.push((f32::from(layout.width), reference_width, ink, mismatched));
        }
        for pair in rows.windows(2) {
            assert!(pair[1].0 > pair[0].0, "widths do not grow with the weight");
            assert!(pair[1].2 > pair[0].2, "ink does not grow with the weight");
        }
        for (weight, (width, reference_width, _, mismatched)) in weights.iter().zip(&rows) {
            assert!(
                (width - reference_width).abs() < 0.01,
                "{weight}: width {width} against {reference_width}"
            );
            assert_eq!(*mismatched, 0, "{weight}: masks differ from the instance's");
        }

        // A `unicode-range` subset declared `font-weight: 300 700` draws 800 at 700.
        let face_family: gpui::SharedString = family.clone().into();
        let composer: gpui::FamilyComposer = Arc::new(move |weight: FontWeight, _| {
            vec![gpui::ComposedFace {
                family: face_family.clone(),
                ranges: Arc::from([(0x0, 0xFF)]),
                weight: Some(FontWeight(weight.0.clamp(300.0, 700.0))),
            }]
        });
        fonts.set_composed_families(vec![("VF Web".into(), composer)]);
        let width = |family: &str, weight: f32| {
            let mut asked = font(family.to_string());
            asked.weight = FontWeight(weight);
            let font_id = fonts.font_id(&asked).unwrap();
            let run = FontRun {
                font_id,
                len: TEXT.len(),
                tracking: px(0.),
                word_spacing: px(0.),
            };
            f32::from(fonts.layout_line(TEXT, px(SIZE), &[run]).width)
        };
        assert_eq!(width("VF Web", 800.0), width(&family, 700.0));
        assert_eq!(width("VF Web", 450.0), width(&family, 450.0));
        assert_eq!(width("VF Web", 200.0), width(&family, 300.0));
    }

    #[test]
    fn test_layout_line_bom_char() {
        let fonts = AppleTextSystem::new();
        let font_id = fonts.font_id(&font("Helvetica")).unwrap();
        let line = "\u{feff}";
        let mut style = FontRun {
            font_id,
            len: line.len(),
            tracking: px(0.),
            word_spacing: px(0.),
        };

        let layout = fonts.layout_line(line, px(16.), &[style]);
        assert_eq!(layout.len, line.len());
        assert!(layout.runs.is_empty());

        let line = "a\u{feff}b";
        style.len = line.len();
        let layout = fonts.layout_line(line, px(16.), &[style]);
        assert_eq!(layout.len, line.len());
        assert_eq!(layout.runs.len(), 1);
        assert_eq!(layout.runs[0].glyphs.len(), 2);
        assert_eq!(layout.runs[0].glyphs[0].id, GlyphId(68u32)); // a
        // There's no glyph for \u{feff}
        assert_eq!(layout.runs[0].glyphs[1].id, GlyphId(69u32)); // b

        let line = "\u{feff}ab";
        let font_runs = &[
            FontRun {
                len: "\u{feff}".len(),
                font_id,
                tracking: px(0.),
                word_spacing: px(0.),
            },
            FontRun {
                len: "ab".len(),
                font_id,
                tracking: px(0.),
                word_spacing: px(0.),
            },
        ];
        let layout = fonts.layout_line(line, px(16.), font_runs);
        assert_eq!(layout.len, line.len());
        assert_eq!(layout.runs.len(), 1);
        assert_eq!(layout.runs[0].glyphs.len(), 2);
        // There's no glyph for \u{feff}
        assert_eq!(layout.runs[0].glyphs[0].id, GlyphId(68u32)); // a
        assert_eq!(layout.runs[0].glyphs[1].id, GlyphId(69u32)); // b
    }

    #[test]
    fn test_layout_line_zwnj_insertion() {
        let fonts = AppleTextSystem::new();
        let font_id = fonts.font_id(&font("Helvetica")).unwrap();

        let text = "hello world";
        let font_runs = &[
            FontRun {
                font_id,
                len: 5,
                tracking: px(0.),
                word_spacing: px(0.),
            }, // "hello"
            FontRun {
                font_id,
                len: 6,
                tracking: px(0.),
                word_spacing: px(0.),
            }, // " world"
        ];

        let layout = fonts.layout_line(text, px(16.), font_runs);
        assert_eq!(layout.len, text.len());

        for run in &layout.runs {
            for glyph in &run.glyphs {
                assert!(
                    glyph.index < text.len(),
                    "Glyph index {} is out of bounds for text length {}",
                    glyph.index,
                    text.len()
                );
            }
        }

        // Test with different font runs - should not insert ZWNJ
        let font_id2 = fonts.font_id(&font("Times")).unwrap_or(font_id);
        let font_runs_different = &[
            FontRun {
                font_id,
                len: 5,
                tracking: px(0.),
                word_spacing: px(0.),
            }, // "hello"
            // " world"
            FontRun {
                font_id: font_id2,
                len: 6,
                tracking: px(0.),
                word_spacing: px(0.),
            },
        ];

        let layout2 = fonts.layout_line(text, px(16.), font_runs_different);
        assert_eq!(layout2.len, text.len());

        for run in &layout2.runs {
            for glyph in &run.glyphs {
                assert!(
                    glyph.index < text.len(),
                    "Glyph index {} is out of bounds for text length {}",
                    glyph.index,
                    text.len()
                );
            }
        }
    }

    #[test]
    fn test_layout_line_zwnj_edge_cases() {
        let fonts = AppleTextSystem::new();
        let font_id = fonts.font_id(&font("Helvetica")).unwrap();

        let text = "hello";
        let font_runs = &[FontRun {
            font_id,
            len: 5,
            tracking: px(0.),
            word_spacing: px(0.),
        }];
        let layout = fonts.layout_line(text, px(16.), font_runs);
        assert_eq!(layout.len, text.len());

        let text = "abc";
        let font_runs = &[
            FontRun {
                font_id,
                len: 1,
                tracking: px(0.),
                word_spacing: px(0.),
            }, // "a"
            FontRun {
                font_id,
                len: 1,
                tracking: px(0.),
                word_spacing: px(0.),
            }, // "b"
            FontRun {
                font_id,
                len: 1,
                tracking: px(0.),
                word_spacing: px(0.),
            }, // "c"
        ];
        let layout = fonts.layout_line(text, px(16.), font_runs);
        assert_eq!(layout.len, text.len());

        for run in &layout.runs {
            for glyph in &run.glyphs {
                assert!(
                    glyph.index < text.len(),
                    "Glyph index {} is out of bounds for text length {}",
                    glyph.index,
                    text.len()
                );
            }
        }

        // Test with empty text
        let text = "";
        let font_runs = &[];
        let layout = fonts.layout_line(text, px(16.), font_runs);
        assert_eq!(layout.len, 0);
        assert!(layout.runs.is_empty());
    }
}
