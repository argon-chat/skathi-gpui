use crate::{Bounds, Corners, Point};
#[cfg(feature = "schemars")]
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// CSS `corner-shape`: the superellipse parameter `K` of `superellipse(K)` (css-borders-4 §3.7).
/// The corner is `x^(2^K) + y^(2^K) = 1`, mirrored about the corner when `K` is negative.
#[derive(Copy, Clone, Debug, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(JsonSchema))]
pub struct CornerShape(pub f32);

impl Default for CornerShape {
    fn default() -> Self {
        Self::ROUND
    }
}

impl CornerShape {
    /// `round`, `superellipse(1)`.
    pub const ROUND: Self = Self(1.);
    /// `squircle`, `superellipse(2)`.
    pub const SQUIRCLE: Self = Self(2.);
    /// `square`, `superellipse(infinity)`.
    pub const SQUARE: Self = Self(f32::INFINITY);
    /// `bevel`, `superellipse(0)`.
    pub const BEVEL: Self = Self(0.);
    /// `scoop`, `superellipse(-1)`.
    pub const SCOOP: Self = Self(-1.);
    /// `notch`, `superellipse(-infinity)`.
    pub const NOTCH: Self = Self(f32::NEG_INFINITY);

    /// Whether this is `round`, the shape `border-radius` alone draws.
    pub fn is_round(self) -> bool {
        self.0 == 1.
    }

    /// The normalized superellipse half corner (§3.9.6): where the curve crosses the corner's
    /// diagonal, from 0 (`notch`) through 0.5 (`bevel`) to 1 (`square`).
    pub fn half_corner(self) -> f32 {
        let k = self.0;
        if k.is_nan() {
            return Self::ROUND.half_corner();
        }
        if k == f32::INFINITY {
            return 1.;
        }
        if k == f32::NEG_INFINITY {
            return 0.;
        }
        let convex = 0.5f32.powf(1. / 2f32.powf(k.abs()));
        if k < 0. { 1. - convex } else { convex }
    }

    /// The shape whose [`CornerShape::half_corner`] is `value`.
    pub fn from_half_corner(value: f32) -> Self {
        let value = value.clamp(0., 1.);
        if value == 0. {
            return Self::NOTCH;
        }
        if value == 1. {
            return Self::SQUARE;
        }
        if value == 0.5 {
            return Self::BEVEL;
        }
        let convex = if value < 0.5 { 1. - value } else { value };
        let k = (0.5f32.ln() / convex.ln()).log2();
        Self(if value < 0.5 { -k } else { k })
    }

    /// CSS interpolation: the half corner moves linearly (§3.9.6).
    pub fn interpolate(self, to: Self, t: f32) -> Self {
        if t <= 0. {
            return self;
        }
        if t >= 1. {
            return to;
        }
        let (a, b) = (self.half_corner(), to.half_corner());
        Self::from_half_corner(a + (b - a) * t)
    }
}

const SHAPE_BITS: u32 = 0xfff;
const SHAPE_STEPS: f32 = 4094.;
const INSET_BITS: u32 = 0x3ff;
const INSET_STEPS_PER_PIXEL: f32 = 4.;

/// The corner shapes of a primitive as the shaders read them. Each corner's word holds the
/// shape's half corner in its low 12 bits, 0 for `round`, then two 10-bit insets in quarter
/// pixels: the corner is the inner edge of a border that wide on the box `bounds` is inside.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Hash)]
#[repr(C)]
pub struct CornerShapes {
    /// The top left corner.
    pub top_left: u32,
    /// The top right corner.
    pub top_right: u32,
    /// The bottom right corner.
    pub bottom_right: u32,
    /// The bottom left corner.
    pub bottom_left: u32,
}

fn shape_code(shape: CornerShape) -> u32 {
    if shape.is_round() {
        return 0;
    }
    1 + (shape.half_corner() * SHAPE_STEPS).round() as u32
}

fn inset_code(inset: f32) -> u32 {
    ((inset.max(0.) * INSET_STEPS_PER_PIXEL).round() as u32).min(INSET_BITS)
}

impl CornerShapes {
    /// Each corner's shape, without insets.
    pub fn new(shapes: &Corners<CornerShape>) -> Self {
        Self {
            top_left: shape_code(shapes.top_left),
            top_right: shape_code(shapes.top_right),
            bottom_right: shape_code(shapes.bottom_right),
            bottom_left: shape_code(shapes.bottom_left),
        }
    }

    /// Whether every corner is round, the shape the shaders draw without these.
    pub fn is_round(&self) -> bool {
        self.words().iter().all(|word| word & SHAPE_BITS == 0)
    }

    fn words(&self) -> [u32; 4] {
        [
            self.top_left,
            self.top_right,
            self.bottom_right,
            self.bottom_left,
        ]
    }

    fn from_words([top_left, top_right, bottom_right, bottom_left]: [u32; 4]) -> Self {
        Self {
            top_left,
            top_right,
            bottom_right,
            bottom_left,
        }
    }

    /// The shapes as the inner edge of a border `insets` wide on each axis, per corner
    /// (x: the left or right border, y: the top or bottom one). A round corner takes none.
    pub fn with_insets(self, insets: Corners<(f32, f32)>) -> Self {
        let insets = [
            insets.top_left,
            insets.top_right,
            insets.bottom_right,
            insets.bottom_left,
        ];
        let mut words = self.words();
        for (word, (x, y)) in words.iter_mut().zip(insets) {
            let shape = *word & SHAPE_BITS;
            *word = if shape == 0 {
                0
            } else {
                shape | inset_code(x) << 12 | inset_code(y) << 22
            };
        }
        Self::from_words(words)
    }

    /// The same shapes without insets.
    pub fn without_insets(self) -> Self {
        Self::from_words(self.words().map(|word| word & SHAPE_BITS))
    }

    /// The insets scaled, as a content mask is scaled to device pixels.
    pub fn scale(self, factor: f32) -> Self {
        Self::from_words(self.words().map(|word| {
            let shape = word & SHAPE_BITS;
            if shape == 0 {
                return 0;
            }
            let (x, y) = word_insets(word);
            shape | inset_code(x * factor) << 12 | inset_code(y * factor) << 22
        }))
    }

    /// The shapes in [`Corners`] order, with each corner's insets.
    pub fn corners(&self) -> Corners<u32> {
        Corners {
            top_left: self.top_left,
            top_right: self.top_right,
            bottom_right: self.bottom_right,
            bottom_left: self.bottom_left,
        }
    }
}

fn word_insets(word: u32) -> (f32, f32) {
    (
        ((word >> 12) & INSET_BITS) as f32 / INSET_STEPS_PER_PIXEL,
        ((word >> 22) & INSET_BITS) as f32 / INSET_STEPS_PER_PIXEL,
    )
}

/// How far `q` is outside the curve of a shaped corner of radius `r`, negative inside: `q` runs
/// inward from the box's corner along both edges, and `inset` is the border on the edge the
/// curve ends on (x: the left or right border). Mirrors `shaped_corner_piece` in the shaders.
pub(crate) fn corner_piece_sdf(q: [f32; 2], r: f32, word: u32, inset: [f32; 2]) -> f32 {
    let code = word & SHAPE_BITS;
    if code == 0 || code == SHAPE_BITS || r <= 0. {
        return f32::MIN;
    }
    let h = (code - 1) as f32 / SHAPE_STEPS;
    let convex = h >= 0.5;
    let (mut a, mut b) = ([r, 0.], [0., r]);
    if inset[0] > 0. || inset[1] > 0. {
        let cp =
            ((1. + std::f32::consts::SQRT_2) * h - std::f32::consts::FRAC_1_SQRT_2).clamp(0., 1.);
        let d = (inset[0] - inset[1]).clamp(-r, r);
        if h <= 0.5 && d.abs() >= r {
            return f32::MIN;
        }
        let mut start = cp;
        if d != 0. {
            let s = (2. * r * r - d * d).sqrt();
            let bevel = (s - d) / (2. * s);
            start = if convex {
                1. - (1. - bevel) * 2. * (1. - cp)
            } else {
                bevel * 2. * cp
            };
        }
        let end = 2. * cp - start;
        let normal = |x: f32, y: f32| {
            let length = (x * x + y * y).sqrt().max(1e-6);
            [x / length, y / length]
        };
        let na = normal(1. - start, start);
        let nb = normal(end, 1. - end);
        a = [a[0] + na[0] * inset[1], a[1] + na[1] * inset[1]];
        b = [b[0] + nb[0] * inset[0], b[1] + nb[1] * inset[0]];
    }
    let axes = [a[0] - b[0], b[1] - a[1]];
    if axes[0] <= 0. || axes[1] <= 0. {
        return f32::MIN;
    }
    let hc = if convex { h } else { 1. - h };
    let n = -std::f32::consts::LN_2 / hc.ln().min(-1e-4);
    let uv = if convex {
        [(a[0] - q[0]) / axes[0], (b[1] - q[1]) / axes[1]]
    } else {
        [(q[0] - b[0]) / axes[0], (q[1] - a[1]) / axes[1]]
    }
    .map(|v| v.max(0.));
    let m = uv[0].max(uv[1]);
    if m <= 0. {
        let depth = axes[0].min(axes[1]);
        return if convex { -depth } else { depth };
    }
    let spow = |x: f32, e: f32| if x <= 0. { 0. } else { (e * x.log2()).exp2() };
    let l = m * spow(spow(uv[0] / m, n) + spow(uv[1] / m, n), 1. / n);
    let slope = |v: f32| if n > 1.0001 { spow(v / l, n - 1.) } else { 1. };
    let g = [slope(uv[0]) / axes[0], slope(uv[1]) / axes[1]];
    let distance = (l - 1.) / (g[0] * g[0] + g[1] * g[1]).sqrt().max(1e-6);
    if convex { distance } else { -distance }
}

/// The signed distance from `point` to a box with shaped corners, negative inside. A corner whose
/// shape is round is gpui's rounded corner.
pub(crate) fn shaped_rect_sdf<P>(
    point: Point<P>,
    bounds: &Bounds<P>,
    radii: &Corners<P>,
    shapes: &CornerShapes,
) -> f32
where
    P: Clone + Copy + std::fmt::Debug + Default + PartialEq + Into<f64>,
{
    let f = |v: P| -> f32 { Into::<f64>::into(v) as f32 };
    let half = [f(bounds.size.width) / 2., f(bounds.size.height) / 2.];
    let to_point = [
        f(point.x) - f(bounds.origin.x) - half[0],
        f(point.y) - f(bounds.origin.y) - half[1],
    ];
    let (r, word) = match (to_point[0] < 0., to_point[1] < 0.) {
        (true, true) => (f(radii.top_left), shapes.top_left),
        (false, true) => (f(radii.top_right), shapes.top_right),
        (false, false) => (f(radii.bottom_right), shapes.bottom_right),
        (true, false) => (f(radii.bottom_left), shapes.bottom_left),
    };
    let corner_to_point = [to_point[0].abs() - half[0], to_point[1].abs() - half[1]];
    if word & SHAPE_BITS == 0 {
        let c = [corner_to_point[0] + r, corner_to_point[1] + r];
        if r == 0. {
            return c[0].max(c[1]);
        }
        let outside = (c[0].max(0.).powi(2) + c[1].max(0.).powi(2)).sqrt();
        return outside + c[0].max(c[1]).min(0.) - r;
    }
    let (ix, iy) = word_insets(word);
    let q = [ix - corner_to_point[0], iy - corner_to_point[1]];
    let rect = corner_to_point[0].max(corner_to_point[1]);
    rect.max(corner_piece_sdf(q, r, word, [ix, iy]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Pixels, point, px, size};

    fn all<T: Clone + std::fmt::Debug + Default + PartialEq>(value: T) -> Corners<T> {
        Corners {
            top_left: value.clone(),
            top_right: value.clone(),
            bottom_right: value.clone(),
            bottom_left: value,
        }
    }

    fn close(a: f32, b: f32, tolerance: f32) {
        assert!(
            (a - b).abs() <= tolerance,
            "{a} is not within {tolerance} of {b}"
        );
    }

    #[test]
    fn keywords_map_to_their_half_corners_and_back() {
        let cases = [
            (CornerShape::NOTCH, 0.),
            (CornerShape::SCOOP, 1. - std::f32::consts::FRAC_1_SQRT_2),
            (CornerShape::BEVEL, 0.5),
            (CornerShape::ROUND, std::f32::consts::FRAC_1_SQRT_2),
            (CornerShape::SQUIRCLE, 0.5f32.powf(0.25)),
            (CornerShape::SQUARE, 1.),
        ];
        for (shape, half) in cases {
            close(shape.half_corner(), half, 1e-6);
            let back = CornerShape::from_half_corner(half);
            assert!(
                back.0 == shape.0 || (back.0 - shape.0).abs() < 1e-4,
                "{back:?} for {shape:?}"
            );
        }
    }

    #[test]
    fn interpolation_moves_the_half_corner_linearly() {
        let mid = CornerShape::BEVEL.interpolate(CornerShape::SQUARE, 0.5);
        close(mid.half_corner(), 0.75, 1e-6);
        let mid = CornerShape::SCOOP.interpolate(CornerShape::ROUND, 0.5);
        close(mid.0, 0., 1e-5);
        assert_eq!(
            CornerShape::NOTCH.interpolate(CornerShape::SQUARE, 0.5),
            CornerShape::BEVEL
        );
        assert_eq!(
            CornerShape::ROUND.interpolate(CornerShape::SCOOP, 1.),
            CornerShape::SCOOP
        );
    }

    #[test]
    fn round_corners_encode_as_zero() {
        let shapes = CornerShapes::new(&all(CornerShape::ROUND)).with_insets(all((4., 4.)));
        assert_eq!(shapes, CornerShapes::default());
        let bevel = CornerShapes::new(&all(CornerShape::BEVEL));
        assert!(!bevel.is_round());
        let inset = bevel.with_insets(all((2.5, 7.))).scale(2.);
        assert_eq!(word_insets(inset.top_left), (5., 14.));
        assert_eq!(inset.without_insets(), bevel);
    }

    fn sdf(x: f32, y: f32, shape: CornerShape, inset: f32) -> f32 {
        let bounds = Bounds::new(
            point(px(inset), px(inset)),
            size(px(100. - 2. * inset), px(100. - 2. * inset)),
        );
        let shapes = CornerShapes::new(&all(shape)).with_insets(all((inset, inset)));
        shaped_rect_sdf::<Pixels>(
            point(px(x), px(y)),
            &bounds,
            &Corners::all(px(40.)),
            &shapes,
        )
    }

    #[test]
    fn each_shape_puts_its_curve_through_its_half_corner() {
        let diagonal = |shape: CornerShape| {
            let along = 40. * (1. - shape.half_corner());
            sdf(along, along, shape, 0.)
        };
        for shape in [
            CornerShape::SCOOP,
            CornerShape::BEVEL,
            CornerShape::SQUIRCLE,
            CornerShape(3.),
            CornerShape(-2.5),
        ] {
            close(diagonal(shape), 0., 0.05);
        }
        close(
            sdf(10., 10., CornerShape::BEVEL, 0.),
            20. / std::f32::consts::SQRT_2,
            1e-3,
        );
        close(sdf(0., 0., CornerShape::SCOOP, 0.), 40., 1e-3);
        close(sdf(28.28427, 28.28427, CornerShape::SCOOP, 0.), 0., 0.01);
        assert!(sdf(39., 39., CornerShape::NOTCH, 0.) > 0.);
        assert!(sdf(41., 20., CornerShape::NOTCH, 0.) < 0.);
        assert!(sdf(1., 1., CornerShape::SQUARE, 0.) < 0.);
    }

    #[test]
    fn an_inset_corner_stays_a_border_width_from_the_outer_curve() {
        for shape in [
            CornerShape::BEVEL,
            CornerShape::SCOOP,
            CornerShape::SQUIRCLE,
            CornerShape(-0.5),
            CornerShape(0.5),
        ] {
            let along = 40. * (1. - shape.half_corner());
            let step = 6. / std::f32::consts::SQRT_2;
            let at = sdf(along + step, along + step, shape, 6.);
            assert!(at.abs() < 1.5, "{shape:?}: {at}");
        }
        close(sdf(46., 6., CornerShape::SCOOP, 6.), 0., 0.3);
    }
}
