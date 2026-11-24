use std::hash::{Hash, Hasher};
use std::sync::Arc;

#[cfg(feature = "schemars")]
use schemars::{JsonSchema, json_schema};
use serde::{Deserialize, Serialize};

use crate::{Font, FontStyle};

/// An OpenType variation axis tag, its four bytes big-endian.
pub const fn axis_tag(bytes: &[u8; 4]) -> u32 {
    u32::from_be_bytes(*bytes)
}

/// The `wght` axis.
pub const WGHT: u32 = axis_tag(b"wght");
/// The `wdth` axis.
pub const WDTH: u32 = axis_tag(b"wdth");
/// The `ital` axis.
pub const ITAL: u32 = axis_tag(b"ital");
/// The `slnt` axis.
pub const SLNT: u32 = axis_tag(b"slnt");

/// The width of a face as a percentage of its normal width, CSS `font-stretch` / `font-width`.
#[derive(Clone, Copy, Debug, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct FontWidth(pub f32);

impl FontWidth {
    /// `ultra-condensed`.
    pub const ULTRA_CONDENSED: FontWidth = FontWidth(50.);
    /// `extra-condensed`.
    pub const EXTRA_CONDENSED: FontWidth = FontWidth(62.5);
    /// `condensed`.
    pub const CONDENSED: FontWidth = FontWidth(75.);
    /// `semi-condensed`.
    pub const SEMI_CONDENSED: FontWidth = FontWidth(87.5);
    /// `normal`.
    pub const NORMAL: FontWidth = FontWidth(100.);
    /// `semi-expanded`.
    pub const SEMI_EXPANDED: FontWidth = FontWidth(112.5);
    /// `expanded`.
    pub const EXPANDED: FontWidth = FontWidth(125.);
    /// `extra-expanded`.
    pub const EXTRA_EXPANDED: FontWidth = FontWidth(150.);
    /// `ultra-expanded`.
    pub const ULTRA_EXPANDED: FontWidth = FontWidth(200.);
}

impl Default for FontWidth {
    fn default() -> Self {
        Self::NORMAL
    }
}

impl Hash for FontWidth {
    fn hash<H: Hasher>(&self, state: &mut H) {
        state.write_u32(self.0.to_bits());
    }
}

impl Eq for FontWidth {}

#[cfg(feature = "schemars")]
impl JsonSchema for FontWidth {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "FontWidth".into()
    }

    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        json_schema!({ "type": "number", "minimum": 0, "default": 100 })
    }
}

/// The angle of `font-style: oblique <angle>` in degrees, positive leaning forward (clockwise).
/// Italic text uses it too, on a face with a `slnt` axis and no `ital` axis.
#[derive(Clone, Copy, Debug, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ObliqueAngle(pub f32);

impl ObliqueAngle {
    /// The angle of `oblique` without one.
    pub const DEFAULT: ObliqueAngle = ObliqueAngle(14.);
}

impl Default for ObliqueAngle {
    fn default() -> Self {
        Self::DEFAULT
    }
}

impl Hash for ObliqueAngle {
    fn hash<H: Hasher>(&self, state: &mut H) {
        state.write_u32(self.0.to_bits());
    }
}

impl Eq for ObliqueAngle {}

#[cfg(feature = "schemars")]
impl JsonSchema for ObliqueAngle {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "ObliqueAngle".into()
    }

    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        json_schema!({ "type": "number", "minimum": -90, "maximum": 90, "default": 14 })
    }
}

/// Coordinates set on variation axes by tag, CSS `font-variation-settings`: they override what
/// the weight, width and style set. One entry per tag, sorted by tag.
#[derive(Clone, PartialEq)]
pub struct FontVariations(pub Arc<Vec<(u32, f32)>>);

impl FontVariations {
    /// `settings` with the last value of each tag kept.
    pub fn new(settings: impl IntoIterator<Item = (u32, f32)>) -> Self {
        let mut list: Vec<(u32, f32)> = Vec::new();
        for (tag, value) in settings {
            match list.iter_mut().find(|(t, _)| *t == tag) {
                Some(slot) => slot.1 = value,
                None => list.push((tag, value)),
            }
        }
        list.sort_by_key(|(tag, _)| *tag);
        if list.is_empty() {
            return Self::default();
        }
        Self(Arc::new(list))
    }

    /// `self` with `over`'s values taking precedence.
    pub fn overridden_by(&self, over: &FontVariations) -> Self {
        if self.0.is_empty() {
            return over.clone();
        }
        if over.0.is_empty() {
            return self.clone();
        }
        Self::new(self.0.iter().chain(over.0.iter()).copied())
    }

    /// The settings, sorted by tag.
    pub fn settings(&self) -> &[(u32, f32)] {
        &self.0
    }

    /// Is no axis set?
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl Default for FontVariations {
    fn default() -> Self {
        static EMPTY: std::sync::LazyLock<Arc<Vec<(u32, f32)>>> =
            std::sync::LazyLock::new(|| Arc::new(Vec::new()));
        Self(EMPTY.clone())
    }
}

impl Eq for FontVariations {}

impl Hash for FontVariations {
    fn hash<H: Hasher>(&self, state: &mut H) {
        state.write_usize(self.0.len());
        for (tag, value) in self.0.iter() {
            state.write_u32(*tag);
            state.write_u32(value.to_bits());
        }
    }
}

impl std::fmt::Debug for FontVariations {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut list = f.debug_map();
        for (tag, value) in self.0.iter() {
            list.entry(&String::from_utf8_lossy(&tag.to_be_bytes()), value);
        }
        list.finish()
    }
}

impl Serialize for FontVariations {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut map = serializer.serialize_map(Some(self.0.len()))?;
        for (tag, value) in self.0.iter() {
            map.serialize_entry(&*String::from_utf8_lossy(&tag.to_be_bytes()), value)?;
        }
        map.end()
    }
}

impl<'de> Deserialize<'de> for FontVariations {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let map = std::collections::BTreeMap::<String, f32>::deserialize(deserializer)?;
        let mut settings = Vec::with_capacity(map.len());
        for (tag, value) in map {
            let bytes: [u8; 4] = tag
                .as_bytes()
                .try_into()
                .map_err(|_| serde::de::Error::custom(format!("bad axis tag {tag:?}")))?;
            settings.push((axis_tag(&bytes), value));
        }
        Ok(Self::new(settings))
    }
}

#[cfg(feature = "schemars")]
impl JsonSchema for FontVariations {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "FontVariations".into()
    }

    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        json_schema!({
            "type": "object",
            "additionalProperties": { "type": "number" }
        })
    }
}

/// One variation axis of a face.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct VariationAxis {
    /// The axis tag.
    pub tag: u32,
    /// The lowest coordinate.
    pub min: f32,
    /// The coordinate of the default instance.
    pub default: f32,
    /// The highest coordinate.
    pub max: f32,
}

impl Font {
    /// The coordinates this font asks of a face with `axes`, for each axis it sets, clamped to
    /// the axis: `wght` from the weight, `wdth` from the width; italic sets `ital` 1, or on a face
    /// without that axis `slnt` at the oblique angle; oblique sets `slnt` (negative leans forward),
    /// or `ital` 1 on a face without `slnt`; normal sets both to 0. `variations` come last and
    /// override.
    pub fn variation_coordinates(&self, axes: &[VariationAxis]) -> Vec<(u32, f32)> {
        let has = |tag| axes.iter().any(|axis| axis.tag == tag);
        let mut wanted: Vec<(u32, f32)> = vec![(WGHT, self.weight.0), (WDTH, self.width.0)];
        let angle = self.oblique_angle.0;
        match self.style {
            FontStyle::Normal => wanted.extend([(ITAL, 0.), (SLNT, 0.)]),
            FontStyle::Italic if has(ITAL) => wanted.push((ITAL, 1.)),
            FontStyle::Italic => wanted.push((SLNT, -angle)),
            FontStyle::Oblique if !has(SLNT) && angle > 0. => wanted.push((ITAL, 1.)),
            FontStyle::Oblique => wanted.push((SLNT, -angle)),
        }
        wanted.extend(self.variations.settings().iter().copied());
        let mut out: Vec<(u32, f32)> = Vec::new();
        for (tag, value) in wanted {
            let Some(axis) = axes.iter().find(|axis| axis.tag == tag) else {
                continue;
            };
            if !value.is_finite() || axis.min > axis.max {
                continue;
            }
            let value = value.clamp(axis.min, axis.max);
            match out.iter_mut().find(|(t, _)| *t == tag) {
                Some(slot) => slot.1 = value,
                None => out.push((tag, value)),
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FontWeight, font};

    fn axis(tag: &[u8; 4], min: f32, default: f32, max: f32) -> VariationAxis {
        VariationAxis {
            tag: axis_tag(tag),
            min,
            default,
            max,
        }
    }

    fn get(coords: &[(u32, f32)], tag: &[u8; 4]) -> Option<f32> {
        coords
            .iter()
            .find(|(t, _)| *t == axis_tag(tag))
            .map(|(_, v)| *v)
    }

    #[test]
    fn a_sloped_style_reaches_the_axis_the_face_has() {
        let flex = [
            axis(b"wght", 100., 400., 1000.),
            axis(b"wdth", 25., 100., 151.),
            axis(b"slnt", -10., 0., 0.),
        ];
        let italic_axis = [axis(b"wght", 300., 400., 700.), axis(b"ital", 0., 0., 1.)];
        let mut f = font("X");
        f.style = FontStyle::Oblique;
        f.oblique_angle = ObliqueAngle(8.);
        assert_eq!(get(&f.variation_coordinates(&flex), b"slnt"), Some(-8.));
        f.oblique_angle = ObliqueAngle(-20.);
        assert_eq!(get(&f.variation_coordinates(&flex), b"slnt"), Some(0.));
        assert_eq!(get(&f.variation_coordinates(&italic_axis), b"ital"), None);
        f.oblique_angle = ObliqueAngle(20.);
        assert_eq!(
            get(&f.variation_coordinates(&italic_axis), b"ital"),
            Some(1.)
        );

        f.style = FontStyle::Italic;
        f.oblique_angle = ObliqueAngle::DEFAULT;
        assert_eq!(get(&f.variation_coordinates(&flex), b"slnt"), Some(-10.));
        assert_eq!(
            get(&f.variation_coordinates(&italic_axis), b"ital"),
            Some(1.)
        );
    }

    #[test]
    fn variation_settings_override_and_are_clamped() {
        let axes = [
            axis(b"wght", 100., 400., 1000.),
            axis(b"wdth", 25., 100., 151.),
            axis(b"GRAD", -200., 0., 150.),
        ];
        let mut f = font("X");
        f.weight = FontWeight(700.);
        f.width = FontWidth(10.);
        f.variations = FontVariations::new([
            (axis_tag(b"wght"), 300.),
            (axis_tag(b"GRAD"), 900.),
            (axis_tag(b"XXXX"), 1.),
        ]);
        let coords = f.variation_coordinates(&axes);
        assert_eq!(get(&coords, b"wght"), Some(300.));
        assert_eq!(get(&coords, b"wdth"), Some(25.));
        assert_eq!(get(&coords, b"GRAD"), Some(150.));
        assert_eq!(get(&coords, b"XXXX"), None);
    }
}
