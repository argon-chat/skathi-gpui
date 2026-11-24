//! vue-native fork: glyph masks drawn from outlines the way macOS draws them: the outline dilated
//! by an amount the text colour selects, optionally hinted vertically, and rasterized as linear
//! area coverage into one 8-bit mask that is blended as is.

use std::cell::RefCell;
use std::sync::atomic::{AtomicU8, Ordering};

use crate::{Bounds, DevicePixels, Rgba, point, size};

/// How the app draws glyphs, chosen once for the process.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum TextRendering {
    /// Outlines dilated by the text colour's luminance and rasterized here as linear coverage,
    /// grayscale, blended without gamma or contrast correction. The platform's own on macOS.
    SkathiMode,
    /// The platform's text: DirectWrite, CoreText or cosmic-text.
    Optimized,
}

impl TextRendering {
    /// `SkathiMode` where Skathi rasterizes glyphs itself, `Optimized` elsewhere.
    pub fn platform_default() -> Self {
        if cfg!(any(
            target_os = "windows",
            target_os = "linux",
            target_os = "freebsd"
        )) {
            TextRendering::SkathiMode
        } else {
            TextRendering::Optimized
        }
    }
}

static TEXT_RENDERING: AtomicU8 = AtomicU8::new(0);

/// Chooses how glyphs are drawn; call it before the first window opens.
pub fn set_text_rendering(mode: TextRendering) {
    let value = match mode {
        TextRendering::SkathiMode => 1,
        TextRendering::Optimized => 2,
    };
    TEXT_RENDERING.store(value, Ordering::Relaxed);
}

/// The mode [`set_text_rendering`] chose, or the platform's default.
pub fn text_rendering() -> TextRendering {
    match TEXT_RENDERING.load(Ordering::Relaxed) {
        1 => TextRendering::SkathiMode,
        2 => TextRendering::Optimized,
        _ => TextRendering::platform_default(),
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Pt {
    x: f64,
    y: f64,
}

impl Pt {
    fn new(x: f64, y: f64) -> Self {
        Self { x, y }
    }
    fn lerp(self, other: Pt, t: f64) -> Pt {
        Pt::new(
            self.x + (other.x - self.x) * t,
            self.y + (other.y - self.y) * t,
        )
    }
    fn offset(self, (dx, dy): (f64, f64)) -> Pt {
        Pt::new(self.x + dx, self.y + dy)
    }
}

#[derive(Clone, Copy, Debug)]
enum Segment {
    Line(Pt, Pt),
    Cubic(Pt, Pt, Pt, Pt),
}

impl Segment {
    fn start(&self) -> Pt {
        match *self {
            Segment::Line(p, _) | Segment::Cubic(p, ..) => p,
        }
    }
    fn end(&self) -> Pt {
        match *self {
            Segment::Line(_, p) | Segment::Cubic(.., p) => p,
        }
    }
    fn reversed(&self) -> Segment {
        match *self {
            Segment::Line(a, b) => Segment::Line(b, a),
            Segment::Cubic(a, b, c, d) => Segment::Cubic(d, c, b, a),
        }
    }
    fn map(&self, f: impl Fn(Pt) -> Pt) -> Segment {
        match *self {
            Segment::Line(a, b) => Segment::Line(f(a), f(b)),
            Segment::Cubic(a, b, c, d) => Segment::Cubic(f(a), f(b), f(c), f(d)),
        }
    }
}

/// A glyph outline in device pixels, relative to the glyph's origin on the baseline, y down.
#[derive(Clone, Debug, Default)]
pub struct GlyphOutline {
    // Stored y up, as the dilation law is stated.
    contours: Vec<Vec<Segment>>,
    start: Option<Pt>,
    current: Option<Pt>,
}

impl GlyphOutline {
    #[allow(missing_docs)]
    pub fn move_to(&mut self, x: f32, y: f32) {
        self.close();
        let p = Pt::new(x as f64, -(y as f64));
        self.start = Some(p);
        self.current = Some(p);
        self.contours.push(Vec::new());
    }

    #[allow(missing_docs)]
    pub fn line_to(&mut self, x: f32, y: f32) {
        let Some(from) = self.current else { return };
        let to = Pt::new(x as f64, -(y as f64));
        if let Some(contour) = self.contours.last_mut() {
            contour.push(Segment::Line(from, to));
        }
        self.current = Some(to);
    }

    #[allow(missing_docs)]
    pub fn cubic_to(&mut self, x1: f32, y1: f32, x2: f32, y2: f32, x: f32, y: f32) {
        let Some(from) = self.current else { return };
        let to = Pt::new(x as f64, -(y as f64));
        if let Some(contour) = self.contours.last_mut() {
            contour.push(Segment::Cubic(
                from,
                Pt::new(x1 as f64, -(y1 as f64)),
                Pt::new(x2 as f64, -(y2 as f64)),
                to,
            ));
        }
        self.current = Some(to);
    }

    #[allow(missing_docs)]
    pub fn quad_to(&mut self, x1: f32, y1: f32, x: f32, y: f32) {
        let Some(from) = self.current else { return };
        let control = Pt::new(x1 as f64, -(y1 as f64));
        let to = Pt::new(x as f64, -(y as f64));
        if let Some(contour) = self.contours.last_mut() {
            contour.push(Segment::Cubic(
                from,
                from.lerp(control, 2.0 / 3.0),
                to.lerp(control, 2.0 / 3.0),
                to,
            ));
        }
        self.current = Some(to);
    }

    #[allow(missing_docs)]
    pub fn close(&mut self) {
        if let (Some(start), Some(current)) = (self.start, self.current)
            && start != current
            && let Some(contour) = self.contours.last_mut()
        {
            contour.push(Segment::Line(current, start));
        }
        self.start = None;
        self.current = None;
    }

    /// Twice the signed area enclosed, counter-clockwise positive with y up.
    fn signed_area2(&self) -> f64 {
        let mut area = 0.0;
        let mut add = |a: Pt, b: Pt| area += a.x * b.y - b.x * a.y;
        for segment in self.contours.iter().flatten() {
            match *segment {
                Segment::Line(a, b) => add(a, b),
                Segment::Cubic(..) => {
                    let mut previous = segment.start();
                    for i in 1..=8 {
                        let next = eval(segment, i as f64 / 8.0);
                        add(previous, next);
                        previous = next;
                    }
                }
            }
        }
        area
    }

    /// The outline grown by `d` device pixels, sideways by `d` on each side and upward by `2d`
    /// so everything resting on the baseline stays on it, appended to `out`.
    fn dilate_into(&self, d: f64, pieces: &mut Vec<Segment>, out: &mut Vec<Segment>) {
        let reverse = self.signed_area2() > 0.0;
        for contour in &self.contours {
            pieces.clear();
            if reverse {
                for segment in contour.iter().rev() {
                    monotone_pieces(&segment.reversed(), pieces);
                }
            } else {
                for segment in contour {
                    monotone_pieces(segment, pieces);
                }
            }
            pieces.retain(|piece| piece.start() != piece.end());
            let Some(first) = pieces.first() else {
                continue;
            };
            let first_start = first.start().offset(piece_offset(first, d));
            let mut pen = first_start;
            for piece in pieces.iter() {
                let offset = piece_offset(piece, d);
                let moved = piece.map(|p| p.offset(offset));
                if moved.start() != pen {
                    out.push(Segment::Line(pen, moved.start()));
                }
                pen = moved.end();
                out.push(moved);
            }
            if pen != first_start {
                out.push(Segment::Line(pen, first_start));
            }
        }
    }
}

/// Where a piece that is monotone in x and y moves: by `d` away from the ink sideways, and by
/// `2d` up when it faces up. Outer contours run clockwise with y up.
fn piece_offset(piece: &Segment, d: f64) -> (f64, f64) {
    let (start, end) = (piece.start(), piece.end());
    let x = if end.y > start.y {
        -d
    } else if end.y < start.y {
        d
    } else {
        0.0
    };
    let y = if end.x > start.x { 2.0 * d } else { 0.0 };
    (x, y)
}

fn eval(segment: &Segment, t: f64) -> Pt {
    match *segment {
        Segment::Line(a, b) => a.lerp(b, t),
        Segment::Cubic(a, b, c, d) => {
            let ab = a.lerp(b, t);
            let bc = b.lerp(c, t);
            let cd = c.lerp(d, t);
            ab.lerp(bc, t).lerp(bc.lerp(cd, t), t)
        }
    }
}

fn split_cubic(segment: Segment, t: f64) -> (Segment, Segment) {
    let Segment::Cubic(a, b, c, d) = segment else {
        unreachable!()
    };
    let ab = a.lerp(b, t);
    let bc = b.lerp(c, t);
    let cd = c.lerp(d, t);
    let abc = ab.lerp(bc, t);
    let bcd = bc.lerp(cd, t);
    let mid = abc.lerp(bcd, t);
    (
        Segment::Cubic(a, ab, abc, mid),
        Segment::Cubic(mid, bcd, cd, d),
    )
}

/// Parameters in (0, 1) where one coordinate of a cubic turns.
fn extrema(p0: f64, p1: f64, p2: f64, p3: f64) -> ([f64; 2], usize) {
    let a = -p0 + 3.0 * p1 - 3.0 * p2 + p3;
    let b = 2.0 * (p0 - 2.0 * p1 + p2);
    let c = p1 - p0;
    let mut roots = [f64::NAN; 2];
    if a.abs() < 1e-12 {
        if b.abs() > 1e-12 {
            roots[0] = -c / b;
        }
    } else {
        let disc = b * b - 4.0 * a * c;
        if disc >= 0.0 {
            let sq = disc.sqrt();
            roots = [(-b + sq) / (2.0 * a), (-b - sq) / (2.0 * a)];
        }
    }
    let inside = |t: f64| t > 1e-10 && t < 1.0 - 1e-10;
    let (lo, hi) = (roots[0].min(roots[1]), roots[0].max(roots[1]));
    match (inside(lo), inside(hi)) {
        (true, true) if lo != hi => ([lo, hi], 2),
        (true, _) => ([lo, 0.0], 1),
        (false, true) => ([hi, 0.0], 1),
        // `min`/`max` skip a NaN, so a single root is in `lo` and `hi` alike.
        (false, false) => ([0.0; 2], 0),
    }
}

fn split_at(segment: Segment, ts: &[f64], out: &mut Vec<Segment>) {
    let mut rest = segment;
    let mut done = 0.0;
    for &t in ts {
        let local = (t - done) / (1.0 - done);
        let (head, tail) = split_cubic(rest, local);
        out.push(head);
        rest = tail;
        done = t;
    }
    out.push(rest);
}

/// Cuts a segment at its x extrema, then each part at its y extrema.
fn monotone_pieces(segment: &Segment, out: &mut Vec<Segment>) {
    let Segment::Cubic(a, b, c, d) = *segment else {
        out.push(*segment);
        return;
    };
    let (ts, n) = extrema(a.x, b.x, c.x, d.x);
    let from = out.len();
    split_at(*segment, &ts[..n], out);
    let by_x: [Segment; 3] =
        std::array::from_fn(|i| out.get(from + i).copied().unwrap_or(*segment));
    out.truncate(from);
    for part in &by_x[..=n] {
        let Segment::Cubic(a, b, c, d) = *part else {
            unreachable!()
        };
        let (ts, n) = extrema(a.y, b.y, c.y, d.y);
        split_at(*part, &ts[..n], out);
    }
}

/// The macOS luminance level of a text colour, 0 to 4: `round(4 Y)` of its linear luminance.
pub fn dilation_level(color: Rgba) -> u8 {
    let decode = |u: f32| {
        let u = u.clamp(0.0, 1.0) as f64;
        if u <= 0.04045 {
            u / 12.92
        } else {
            ((u + 0.055) / 1.055).powf(2.4)
        }
    };
    let y = 0.2126 * decode(color.r) + 0.7152 * decode(color.g) + 0.0722 * decode(color.b);
    (4.0 * y).round().clamp(0.0, 4.0) as u8
}

/// The dilation in device pixels for a size in device pixels per em and a luminance level.
pub fn dilation_amount(ppem: f32, level: u8) -> f32 {
    let q = level.min(4) as f64 / 4.0;
    (ppem as f64 * (0.0058 + 0.01 * q)).min(0.3f32 as f64) as f32
}

/// What the vertical autohint reads from a font, in font units.
#[derive(Clone, Copy, Debug)]
pub struct HintMetrics {
    #[allow(missing_docs)]
    pub units_per_em: f32,
    /// The top of the lowercase `o`.
    pub x_height_top: f32,
    /// The top of the capital `X`.
    pub cap_top: f32,
    /// The top of the font's bounding box.
    pub box_top: f32,
    /// A declared horizontal stem thickness, normalized to a 1000-unit em; 0 when none.
    pub stem: f32,
}

/// A vertical, metrics-driven remapping of y that puts the x-height and the cap height on pixel
/// lines while keeping the baseline and the thickness of horizontal stems at the cap line.
#[derive(Clone, Copy, Debug)]
pub struct VerticalHint {
    /// `(from, to)` knots in device pixels above the baseline; identity outside them.
    knots: [(f64, f64); 5],
}

impl VerticalHint {
    /// The map for a size in device pixels per em and a dilation in device pixels, or `None`
    /// where macOS does not hint: a fractional size, or one outside 7 to 18 ppem.
    pub fn new(metrics: &HintMetrics, ppem: f32, dilation: f32) -> Option<Self> {
        if !Self::applies(ppem) {
            return None;
        }
        let p = ppem.round() as f64;
        let o = metrics.x_height_top.ceil() as f64;
        let x = metrics.cap_top.ceil() as f64;
        if metrics.units_per_em <= 0.0 || o <= 0.0 || x <= 0.0 || o == x {
            return None;
        }
        let u = p / metrics.units_per_em as f64;
        let d = dilation as f64;
        let stem = if metrics.stem > 0.0 {
            metrics.stem as f64
        } else {
            x
        };
        let t = u * stem.min(0.333 * o).min((x - o) / 2.0);
        let a = d + t;
        let b = u * o + d;
        let c = u * x - t - d;
        let z = u * x + d;
        let top = u * metrics.box_top as f64 + d;
        if !(a < b && b < c && c < z && z < top) {
            return None;
        }
        let nominal = (0.015125 * p).min(0.3f32 as f64);
        let slopes = |to: f64, tx: f64| {
            let b_to = to - d;
            let c_to = tx - t - 3.0 * d;
            ((b_to - a) / (b - a), (c_to - b_to) / (c - b), b_to, c_to)
        };
        // CoreText rounds the tops with the nominal dilation added once, as the knots `b` and
        // `z` carry it.
        let candidates = [
            ((u * o + nominal).round(), (u * x + nominal).round()),
            ((u * o).round(), (u * x).round()),
        ];
        let mut best: Option<(f64, f64, f64)> = None;
        for (to, tx) in candidates {
            let (s1, s2, ..) = slopes(to, tx);
            let cost = (s1 - 1.0).abs() + (s2 - 1.0).abs();
            if best.is_none_or(|(best_cost, ..)| cost < best_cost) {
                best = Some((cost, to, tx));
            }
        }
        let (_, to, tx) = best?;
        let (s1, mut s2, b_to, mut c_to) = slopes(to, tx);
        let mut z_to = tx - d;
        if s2 < 0.75 || (s2 - s1).abs() > 0.3 {
            c_to = c;
            z_to = z;
            s2 = (c - b_to) / (c - b);
        }
        if o / x > 0.85 && s2 < 0.33 {
            return None;
        }
        if !(a < b_to && b_to < c_to && c_to < z_to && z_to < top) {
            return None;
        }
        Some(Self {
            knots: [(a, a), (b, b_to), (c, c_to), (z, z_to), (top, top)],
        })
    }

    /// Whether a size in device pixels per em is one macOS hints: a whole number from 7 to 18.
    pub fn applies(ppem: f32) -> bool {
        (ppem - ppem.round()).abs() <= 1e-4 && (7.0..=18.0).contains(&ppem.round())
    }

    fn map(&self, y: f64) -> f64 {
        let k = &self.knots;
        if y <= k[0].0 || y >= k[4].0 {
            return y;
        }
        for pair in k.windows(2) {
            let ((x0, y0), (x1, y1)) = (pair[0], pair[1]);
            if y <= x1 {
                return y0 + (y - x0) * (y1 - y0) / (x1 - x0);
            }
        }
        y
    }
}

/// A coverage mask and where it sits relative to the glyph origin, y down.
#[derive(Clone, Debug, Default)]
pub struct CoverageMask {
    #[allow(missing_docs)]
    pub bounds: Bounds<DevicePixels>,
    /// One byte per pixel, rows top down.
    pub bytes: Vec<u8>,
}

#[derive(Default)]
struct Scratch {
    pieces: Vec<Segment>,
    segments: Vec<Segment>,
    lines: Vec<(Pt, Pt)>,
    cells: Vec<f32>,
}

thread_local! {
    static SCRATCH: RefCell<Scratch> = RefCell::new(Scratch::default());
}

/// Dilates `outline` by `dilation` device pixels, moves it `x_offset` pixels right, applies
/// `hint`, and rasterizes the result.
pub fn rasterize_dilated(
    outline: &GlyphOutline,
    x_offset: f32,
    dilation: f32,
    hint: Option<&VerticalHint>,
) -> CoverageMask {
    SCRATCH.with_borrow_mut(|scratch| {
        let Scratch {
            pieces,
            segments,
            lines,
            cells,
        } = scratch;
        segments.clear();
        lines.clear();
        if dilation > 0.0 {
            outline.dilate_into(dilation as f64, pieces, segments);
        } else {
            segments.extend(outline.contours.iter().flatten().copied());
        }
        let dx = x_offset as f64;
        for segment in segments.iter() {
            let segment = segment.map(|p| Pt::new(p.x + dx, hint.map_or(p.y, |h| h.map(p.y))));
            match segment {
                Segment::Line(a, b) => lines.push((a, b)),
                Segment::Cubic(a, b, c, d) => flatten_cubic([a, b, c, d], 0, lines),
            }
        }
        rasterize_lines(lines, cells)
    })
}

/// Midpoint subdivision until the second differences of the control points are under about a
/// quarter pixel on both axes.
fn flatten_cubic(p: [Pt; 4], depth: u32, out: &mut Vec<(Pt, Pt)>) {
    let dd = |a: Pt, b: Pt, c: Pt| {
        let (x, y) = (a.x - 2.0 * b.x + c.x, a.y - 2.0 * b.y + c.y);
        (x * x).max(y * y)
    };
    if depth >= 22 || dd(p[0], p[1], p[2]).max(dd(p[1], p[2], p[3])) <= 0.0711 {
        out.push((p[0], p[3]));
        return;
    }
    let (head, tail) = split_cubic(Segment::Cubic(p[0], p[1], p[2], p[3]), 0.5);
    for half in [head, tail] {
        let Segment::Cubic(a, b, c, d) = half else {
            unreachable!()
        };
        flatten_cubic([a, b, c, d], depth + 1, out);
    }
}

/// Signed-area accumulation: each edge deposits its exact area into the cells of the rows it
/// crosses, a running sum along each row gives the coverage, and opposite windings cancel
/// before the absolute value is taken. A byte is `floor(256 area)`, clamped to 255.
fn rasterize_lines(lines: &[(Pt, Pt)], cells: &mut Vec<f32>) -> CoverageMask {
    let (mut min_x, mut min_y) = (f64::INFINITY, f64::INFINITY);
    let (mut max_x, mut max_y) = (f64::NEG_INFINITY, f64::NEG_INFINITY);
    for (a, b) in lines {
        for p in [a, b] {
            min_x = min_x.min(p.x);
            max_x = max_x.max(p.x);
            min_y = min_y.min(p.y);
            max_y = max_y.max(p.y);
        }
    }
    if !(min_x < max_x && min_y < max_y) {
        return CoverageMask::default();
    }
    // A sliver under 1/256 px past a pixel line, as the hint leaves at a snapped top, covers no
    // byte: it does not open a row or column of its own.
    const EPSILON: f64 = 1.0 / 256.0;
    let x0 = (min_x + EPSILON).floor();
    let top = (max_y - EPSILON).ceil();
    let width = ((max_x - EPSILON).ceil() - x0).max(1.0) as usize;
    let height = (top - (min_y + EPSILON).floor()).max(1.0) as usize;
    let stride = width + 2;
    cells.clear();
    cells.resize(stride * height + 1, 0.0);
    let (w, h) = (width as f64, height as f64);
    for (a, b) in lines {
        accumulate(
            cells,
            stride,
            height,
            (a.x - x0).clamp(0.0, w) as f32,
            (top - a.y).clamp(0.0, h) as f32,
            (b.x - x0).clamp(0.0, w) as f32,
            (top - b.y).clamp(0.0, h) as f32,
        );
    }
    let mut bytes = vec![0u8; width * height];
    for row in 0..height {
        let mut sum = 0f32;
        for column in 0..width {
            sum += cells[row * stride + column];
            bytes[row * width + column] = (sum.abs() * 256.0).min(255.0) as u8;
        }
    }
    CoverageMask {
        bounds: Bounds::new(
            point(DevicePixels(x0 as i32), DevicePixels(-top as i32)),
            size(DevicePixels(width as i32), DevicePixels(height as i32)),
        ),
        bytes,
    }
}

/// Adds the area under one edge, in bitmap coordinates with rows down, to the cells it covers.
fn accumulate(cells: &mut [f32], stride: usize, height: usize, ax: f32, ay: f32, bx: f32, by: f32) {
    if ay == by {
        return;
    }
    let (dir, x_top, y_top, x_bottom, y_bottom) = if ay < by {
        (1.0, ax, ay, bx, by)
    } else {
        (-1.0, bx, by, ax, ay)
    };
    let dxdy = (x_bottom - x_top) / (y_bottom - y_top);
    let first_row = y_top.max(0.0) as usize;
    let last_row = (y_bottom.ceil() as usize).min(height);
    let mut x = x_top + dxdy * ((first_row as f32).max(y_top) - y_top);
    for row in first_row..last_row {
        let line = row * stride;
        let dy = ((row + 1) as f32).min(y_bottom) - (row as f32).max(y_top);
        let x_next = x + dxdy * dy;
        let d = dy * dir;
        let (left, right) = if x < x_next { (x, x_next) } else { (x_next, x) };
        let left_floor = left.floor();
        let left_cell = left_floor.max(0.0) as usize;
        let right_ceil = right.ceil();
        let right_cell = right_ceil.max(0.0) as usize;
        if right_cell <= left_cell + 1 {
            let mid = 0.5 * (x + x_next) - left_floor;
            cells[line + left_cell] += d - d * mid;
            cells[line + left_cell + 1] += d * mid;
        } else {
            let s = 1.0 / (right - left);
            let left_frac = left - left_floor;
            let a0 = 0.5 * s * (1.0 - left_frac) * (1.0 - left_frac);
            let right_frac = right - right_ceil + 1.0;
            let am = 0.5 * s * right_frac * right_frac;
            cells[line + left_cell] += d * a0;
            if right_cell == left_cell + 2 {
                cells[line + left_cell + 1] += d * (1.0 - a0 - am);
            } else {
                let a1 = s * (1.5 - left_frac);
                cells[line + left_cell + 1] += d * (a1 - a0);
                for cell in left_cell + 2..right_cell - 1 {
                    cells[line + cell] += d * s;
                }
                let a2 = a1 + (right_cell - left_cell - 3) as f32 * s;
                cells[line + right_cell - 1] += d * (1.0 - a2 - am);
            }
            cells[line + right_cell] += d * am;
        }
        x = x_next;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(outline: &mut GlyphOutline, x0: f32, y0: f32, x1: f32, y1: f32, clockwise: bool) {
        // y down, as callers pass outlines.
        outline.move_to(x0, y0);
        if clockwise {
            outline.line_to(x1, y0);
            outline.line_to(x1, y1);
            outline.line_to(x0, y1);
        } else {
            outline.line_to(x0, y1);
            outline.line_to(x1, y1);
            outline.line_to(x1, y0);
        }
        outline.close();
    }

    fn dilated_extent(outline: &GlyphOutline, d: f64) -> (f64, f64, f64, f64) {
        let mut segments = Vec::new();
        outline.dilate_into(d, &mut Vec::new(), &mut segments);
        let mut e = (
            f64::INFINITY,
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::NEG_INFINITY,
        );
        for segment in &segments {
            for p in [segment.start(), segment.end()] {
                e = (e.0.min(p.x), e.1.min(p.y), e.2.max(p.x), e.3.max(p.y));
            }
        }
        e
    }

    fn near(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    #[test]
    fn dilation_grows_sideways_by_d_and_upward_by_2d_with_the_baseline_kept() {
        for clockwise in [true, false] {
            let mut outline = GlyphOutline::default();
            rect(&mut outline, 0.0, -20.0, 10.0, 0.0, clockwise);
            let (x0, y0, x1, y1) = dilated_extent(&outline, 0.2);
            assert!(near(x0, -0.2) && near(x1, 10.2), "{x0} {x1}");
            assert!(near(y0, 0.0) && near(y1, 20.4), "{y0} {y1}");
        }
    }

    #[test]
    fn a_counter_shrinks_the_way_its_walls_grow() {
        // A ring: the hole's walls are ink edges facing inward, so it narrows by 2d and its
        // floor, the top of the lower stroke, rises by 2d while its roof stays.
        let mut outline = GlyphOutline::default();
        rect(&mut outline, 0.0, -10.0, 10.0, 0.0, true);
        rect(&mut outline, 3.0, -7.0, 7.0, -3.0, false);
        let mask = rasterize_dilated(&outline, 0.0, 0.25, None);
        let width = mask.bounds.size.width.0;
        let top = -mask.bounds.origin.y.0;
        // The pixel from `x` to `x + 1` and from `y` to `y + 1` above the baseline.
        let at = |x: i32, y: i32| {
            mask.bytes[((top - y - 1) * width + x - mask.bounds.origin.x.0) as usize]
        };
        assert_eq!(at(5, 3), 128, "floor raised by 2d");
        assert_eq!(at(5, 6), 0, "roof kept");
        assert_eq!(at(3, 5), 64, "left wall moved right by d");
        assert_eq!(at(6, 5), 64, "right wall moved left by d");
        assert_eq!(at(-1, 5), 64, "outside grown by d");
        assert_eq!(at(5, 10), 128, "top grown by 2d");
        assert_eq!(mask.bounds.origin.y.0 + mask.bounds.size.height.0, 0);
    }

    #[test]
    fn a_dilated_curve_keeps_its_bottom_on_the_baseline() {
        // A circle of radius 5 resting on the baseline, from four cubics.
        let k = 5.0 * 0.5523;
        let mut outline = GlyphOutline::default();
        outline.move_to(0.0, -5.0);
        outline.cubic_to(0.0, -5.0 - k, 5.0 - k, -10.0, 5.0, -10.0);
        outline.cubic_to(5.0 + k, -10.0, 10.0, -5.0 - k, 10.0, -5.0);
        outline.cubic_to(10.0, -5.0 + k, 5.0 + k, 0.0, 5.0, 0.0);
        outline.cubic_to(5.0 - k, 0.0, 0.0, -5.0 + k, 0.0, -5.0);
        outline.close();
        let d = 0.3;
        let mask = rasterize_dilated(&outline, 0.0, d, None);
        let plain = rasterize_dilated(&outline, 0.0, 0.0, None);
        assert_eq!(mask.bounds.origin.y.0 + mask.bounds.size.height.0, 0);
        let area = |m: &CoverageMask| m.bytes.iter().map(|&b| b as f64 / 256.0).sum::<f64>();
        // A convex outline becomes its sum with a 2d by 2d box: 2d times its height plus 2d
        // times its width, plus the box.
        let gained = area(&mask) - area(&plain);
        let d = d as f64;
        let expected = 2.0 * d * 10.0 * 2.0 + 4.0 * d * d;
        assert!(
            (gained - expected).abs() < 0.02 * expected,
            "{gained} vs {expected}"
        );
    }

    #[test]
    fn coverage_is_linear_in_area() {
        let mut outline = GlyphOutline::default();
        rect(&mut outline, 0.25, -2.75, 2.75, -0.25, true);
        let mask = rasterize_dilated(&outline, 0.0, 0.0, None);
        assert_eq!(mask.bounds.size.width.0, 3);
        assert_eq!(
            mask.bytes,
            [144, 192, 144, 192, 255, 192, 144, 192, 144],
            "{:?}",
            mask.bytes
        );
        for (w, byte) in [(0.25, 64), (0.5, 128)] {
            let mut outline = GlyphOutline::default();
            rect(&mut outline, 0.0, -1.0, w, 0.0, true);
            assert_eq!(rasterize_dilated(&outline, 0.0, 0.0, None).bytes, [byte]);
        }
    }

    #[test]
    fn an_offset_mask_is_the_mask_of_the_moved_outline() {
        let mut at_origin = GlyphOutline::default();
        let mut moved = GlyphOutline::default();
        for (outline, dx) in [(&mut at_origin, 0.0), (&mut moved, 0.37)] {
            outline.move_to(1.0 + dx, 0.0);
            outline.quad_to(4.0 + dx, -9.0, 7.0 + dx, 0.0);
            outline.close();
        }
        let a = rasterize_dilated(&at_origin, 0.37, 0.2, None);
        let b = rasterize_dilated(&moved, 0.0, 0.2, None);
        assert_eq!(a.bounds, b.bounds);
        assert!(
            a.bytes
                .iter()
                .zip(&b.bytes)
                .all(|(x, y)| x.abs_diff(*y) <= 1)
        );
    }

    #[test]
    fn the_level_comes_from_linear_luminance() {
        let level = |r: f32, g: f32, b: f32| dilation_level(Rgba { r, g, b, a: 1.0 });
        assert_eq!(
            [
                level(0.0, 0.0, 0.0),
                level(1.0, 0.0, 0.0),
                level(0.0, 1.0, 0.0),
                level(0.0, 0.0, 1.0),
                level(0.5, 0.5, 0.5),
                level(0.75, 0.75, 0.75),
                level(1.0, 1.0, 1.0),
            ],
            [0, 1, 3, 0, 1, 2, 4]
        );
        for (ppem, level, d) in [(13.0, 0, 0.0754), (13.0, 4, 0.2054), (26.0, 1, 0.2158)] {
            assert!((dilation_amount(ppem, level) - d).abs() < 1e-4);
        }
        assert_eq!(dilation_amount(26.0, 4), 0.3f32);
    }

    #[test]
    fn the_hint_puts_the_x_height_and_cap_top_on_pixels_as_macos_does() {
        // The system font's 17 pt optical instance at 18 ppem, black and white.
        let metrics = HintMetrics {
            units_per_em: 2048.0,
            x_height_top: 1097.0,
            cap_top: 1443.0,
            box_top: 1960.0,
            stem: 79.0,
        };
        for (d, b_to, c_to, z_to, slope_above) in [
            (0.1044, 9.8956, 11.9925, 12.8956, 0.9761),
            (0.2844, 9.7156, 11.4525, 12.7156, 1.0553),
        ] {
            let hint = VerticalHint::new(&metrics, 18.0, d as f32).unwrap();
            let k = hint.knots;
            let close = |a: f64, b: f64| (a - b).abs() < 2e-4;
            assert!(
                close(k[1].1, b_to) && close(k[2].1, c_to) && close(k[3].1, z_to),
                "{k:?}"
            );
            let slope = (k[4].1 - k[3].1) / (k[4].0 - k[3].0);
            assert!(close(slope, slope_above), "{slope}");
            assert_eq!(hint.map(-3.0), -3.0);
        }
        // Manrope 400, no declared stem, white text at 13 ppem: CoreText snaps the x-height to
        // 7 px and the cap top to 9, as it does for black.
        let manrope = HintMetrics {
            units_per_em: 2000.0,
            x_height_top: 1110.0,
            cap_top: 1440.0,
            box_top: 1995.0,
            stem: 0.0,
        };
        for d in [dilation_amount(13.0, 0), dilation_amount(13.0, 4)] {
            let k = VerticalHint::new(&manrope, 13.0, d).unwrap().knots;
            let d = d as f64;
            assert!(
                (k[1].1 - (7.0 - d)).abs() < 1e-6 && (k[3].1 - (9.0 - d)).abs() < 1e-6,
                "{k:?}"
            );
        }
        assert!(VerticalHint::new(&metrics, 18.5, 0.1).is_none());
        assert!(VerticalHint::new(&metrics, 19.0, 0.1).is_none());
    }
}
