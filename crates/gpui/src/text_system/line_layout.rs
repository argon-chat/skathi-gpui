use crate::{
    FontAspect, FontId, GlyphId, Hyphens, LineBoxRules, LineLeading, LineMetrics, Pixels, Point,
    SharedString, Size, TabSize, TextSystem, TextWrap, WordBreak, point, px,
};
use collections::{FxHashMap, FxHashSet};
use icu_segmenter::{
    LineSegmenter,
    options::{LineBreakOptions, LineBreakWordOption},
};
use itertools::Itertools;
use smallvec::SmallVec;
use std::{
    borrow::Borrow,
    cell::RefCell,
    hash::{Hash, Hasher},
    ops::Range,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use super::LineWrapper;

/// vue-native fork: line lookups and the shaping they missed into, process-wide.
/// One relaxed add per lookup and a clock pair per shape, so an embedder's
/// frame trace can say how much of a frame went to text shaping.
static LINE_LOOKUPS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static LINE_SHAPES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static LINE_SHAPE_NANOS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// vue-native fork: `(lookups, shapes, nanoseconds spent shaping)` since the
/// process started, over every window's line layout cache. A lookup that is
/// not a shape was answered from this frame's or the previous frame's cache.
pub fn line_layout_counters() -> (u64, u64, u64) {
    (
        LINE_LOOKUPS.load(Ordering::Relaxed),
        LINE_SHAPES.load(Ordering::Relaxed),
        LINE_SHAPE_NANOS.load(Ordering::Relaxed),
    )
}

fn count_lookup() {
    LINE_LOOKUPS.fetch_add(1, Ordering::Relaxed);
}

fn count_shape(started: std::time::Instant) {
    LINE_SHAPES.fetch_add(1, Ordering::Relaxed);
    LINE_SHAPE_NANOS.fetch_add(started.elapsed().as_nanos() as u64, Ordering::Relaxed);
}

/// A laid out and styled line of text
#[derive(Default, Debug)]
pub struct LineLayout {
    /// The font size for this line
    pub font_size: Pixels,
    /// The width of the line
    pub width: Pixels,
    /// The ascent of the line
    pub ascent: Pixels,
    /// The descent of the line
    pub descent: Pixels,
    /// The shaped runs that make up this line
    pub runs: Vec<ShapedRun>,
    /// The length of the line in utf-8 bytes
    pub len: usize,
}

/// A run of text that has been shaped .
#[derive(Debug, Clone)]
pub struct ShapedRun {
    /// The font id for this run
    pub font_id: FontId,
    /// vue-native fork: the font size this run was shaped at. A platform text system leaves
    /// it, `ascent` and `descent` as [`ShapedRun::new`] does; the layout cache fills them in.
    pub font_size: Pixels,
    /// vue-native fork: the run's extent above the baseline, at its own size.
    pub ascent: Pixels,
    /// vue-native fork: the run's extent below the baseline, at its own size.
    pub descent: Pixels,
    /// vue-native fork: the run holds the place of an [`InlineBox`]; its one glyph is not painted.
    pub inline_box: bool,
    /// vue-native fork: set for a run in a font other than the one its text asked for, when that
    /// changes the line: see [`FallbackPlace`]. The layout cache fills it in.
    pub fallback: Option<FallbackPlace>,
    /// The glyphs that make up this run
    pub glyphs: Vec<ShapedGlyph>,
}

/// vue-native fork: the place in its line of a run drawn with a fallback font. The run stands where
/// the font its text asked for would, at the size the text asked for, and under CSS
/// `line-height: normal` reaches as much further as its own font's box does.
#[derive(Copy, Clone, Debug, Default, PartialEq)]
pub struct FallbackPlace {
    /// The size the text asked for. The run's own `font_size` differs under `font-size-adjust`.
    pub size: Pixels,
    /// The asked font's ascent at `size`.
    pub ascent: Pixels,
    /// The asked font's descent at `size`.
    pub descent: Pixels,
    /// How much further than the asked font the run's own font reaches above the baseline.
    pub grow_above: Pixels,
    /// How much further than the asked font the run's own font reaches below the baseline.
    pub grow_below: Pixels,
}

impl ShapedRun {
    /// A run of glyphs in `font_id`, as a platform text system produces it.
    pub fn new(font_id: FontId, glyphs: Vec<ShapedGlyph>) -> Self {
        Self {
            font_id,
            font_size: px(0.),
            ascent: px(0.),
            descent: px(0.),
            inline_box: false,
            fallback: None,
            glyphs,
        }
    }

    pub(crate) fn with_glyphs(&self, glyphs: Vec<ShapedGlyph>) -> Self {
        Self {
            font_id: self.font_id,
            font_size: self.font_size,
            ascent: self.ascent,
            descent: self.descent,
            inline_box: self.inline_box,
            fallback: self.fallback,
            glyphs,
        }
    }
}

/// vue-native fork: room reserved in a line for an element placed there, such as an
/// editor's atom. The run it is set on lays out as one unbreakable glyph of this size.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct InlineBox {
    /// The advance the box takes in the line.
    pub width: Pixels,
    /// How far the box reaches above the baseline.
    pub ascent: Pixels,
    /// How far the box reaches below the baseline.
    pub descent: Pixels,
}

/// vue-native fork: where one visual line of a layout sits, relative to the top of the layout.
#[derive(Copy, Clone, Debug, Default, PartialEq)]
pub struct LineBox {
    /// The line's top.
    pub top: Pixels,
    /// The line's height.
    pub height: Pixels,
    /// The baseline's distance from the line's top.
    pub baseline: Pixels,
}

/// vue-native fork: a font's box under `line-metrics: snapped`, from its unrounded ascent and
/// descent: each rounded to a whole pixel, ties away from zero, and the descent rounded up instead
/// when rounding would clip the descender.
pub fn snapped_line_extent(ascent: Pixels, descent: Pixels) -> (Pixels, Pixels) {
    let rounded = descent.0.round();
    let descent = if descent.0 - rounded > 1e-4 {
        descent.0.ceil()
    } else {
        rounded
    };
    (px(ascent.0.round()), px(descent))
}

/// vue-native fork: the baseline's distance from the top of a line `line_height` tall, for a
/// font box reaching `ascent` above the baseline and `descent` below.
pub fn line_baseline(
    line_height: Pixels,
    ascent: Pixels,
    descent: Pixels,
    leading: LineLeading,
) -> Pixels {
    match leading {
        LineLeading::Split => (line_height - ascent - descent) / 2. + ascent,
        LineLeading::Above => line_height - descent,
    }
}

/// How far a line `line_height` tall reaches above and below the baseline of a box `ascent` and
/// `descent` tall.
fn line_extent(
    line_height: Pixels,
    ascent: Pixels,
    descent: Pixels,
    leading: LineLeading,
) -> (Pixels, Pixels) {
    match leading {
        LineLeading::Split => {
            let half_leading = (line_height - ascent - descent) / 2.;
            (ascent + half_leading, descent + half_leading)
        }
        LineLeading::Above => (line_height - descent, descent),
    }
}

/// vue-native fork: the font `runs` ask for at byte `index`.
fn asked_font(runs: &[FontRun], index: usize) -> FontId {
    let mut end = 0;
    for run in runs {
        end += run.len;
        if index < end {
            return run.font_id;
        }
    }
    runs.last().map_or(FontId(0), |run| run.font_id)
}

/// vue-native fork: the size `spans` ask for at byte `index`, `font_size` without spans.
fn asked_size(spans: &[LayoutSpan], font_size: Pixels, index: usize) -> Pixels {
    let mut end = 0;
    for span in spans {
        end += span.len;
        if index < end {
            return span.font_size;
        }
    }
    spans.last().map_or(font_size, |span| span.font_size)
}

/// The visual lines of a layout: all alike unless its runs differ in size or hold inline boxes.
pub(crate) enum LineBoxes {
    Uniform { height: Pixels, baseline: Pixels },
    Mixed(SmallVec<[LineBox; 1]>),
}

impl LineBoxes {
    pub(crate) fn get(&self, ix: usize) -> LineBox {
        match self {
            Self::Uniform { height, baseline } => LineBox {
                top: *height * ix as f32,
                height: *height,
                baseline: *baseline,
            },
            Self::Mixed(boxes) => boxes.get(ix).copied().unwrap_or_else(|| {
                let last = boxes.last().copied().unwrap_or_default();
                LineBox {
                    top: last.top + last.height * (ix + 1 - boxes.len()) as f32,
                    ..last
                }
            }),
        }
    }

    /// The visual line that `y` falls in, counting lines past the last as uniform ones would.
    fn line_at(&self, y: Pixels) -> usize {
        match self {
            Self::Uniform { height, .. } => (y / *height) as usize,
            Self::Mixed(boxes) => boxes
                .iter()
                .position(|line| y < line.top + line.height)
                .unwrap_or(boxes.len()),
        }
    }

    fn total_height(&self, lines: usize) -> Pixels {
        match self {
            Self::Uniform { height, .. } => *height * lines as f32,
            Self::Mixed(boxes) => boxes.last().map_or(px(0.), |line| line.top + line.height),
        }
    }
}

/// A single glyph, ready to paint.
#[derive(Clone, Debug)]
pub struct ShapedGlyph {
    /// The ID for this glyph, as determined by the text system.
    pub id: GlyphId,

    /// The position of this glyph in its containing line.
    pub position: Point<Pixels>,

    /// The index of this glyph in the original text.
    pub index: usize,

    /// Whether this glyph is an emoji
    pub is_emoji: bool,
}

impl LineLayout {
    /// The index for the character at the given x coordinate
    pub fn index_for_x(&self, x: Pixels) -> Option<usize> {
        if x >= self.width {
            None
        } else {
            for run in self.runs.iter().rev() {
                for glyph in run.glyphs.iter().rev() {
                    if glyph.position.x <= x {
                        return Some(glyph.index);
                    }
                }
            }
            Some(0)
        }
    }

    /// closest_index_for_x returns the character boundary closest to the given x coordinate
    /// (e.g. to handle aligning up/down arrow keys)
    pub fn closest_index_for_x(&self, x: Pixels) -> usize {
        let mut prev_index = 0;
        let mut prev_x = px(0.);

        for run in self.runs.iter() {
            for glyph in run.glyphs.iter() {
                if glyph.position.x >= x {
                    if glyph.position.x - x < x - prev_x {
                        return glyph.index;
                    } else {
                        return prev_index;
                    }
                }
                prev_index = glyph.index;
                prev_x = glyph.position.x;
            }
        }

        if self.len == 1 {
            if x > self.width / 2. {
                return 1;
            } else {
                return 0;
            }
        }

        self.len
    }

    /// The x position of the character at the given index
    pub fn x_for_index(&self, index: usize) -> Pixels {
        for run in &self.runs {
            for glyph in &run.glyphs {
                if glyph.index >= index {
                    return glyph.position.x;
                }
            }
        }
        self.width
    }

    /// The corresponding Font at the given index
    pub fn font_id_for_index(&self, index: usize) -> Option<FontId> {
        for run in &self.runs {
            for glyph in &run.glyphs {
                if glyph.index >= index {
                    return Some(run.font_id);
                }
            }
        }
        None
    }

    /// vue-native fork: whether a run differs from the line in font size, is an inline box or
    /// holds a fallback font's place, which makes each visual line as tall as what it holds.
    pub fn has_mixed_runs(&self) -> bool {
        self.runs
            .iter()
            .any(|run| run.inline_box || run.fallback.is_some() || run.font_size != self.font_size)
    }

    /// vue-native fork: the visual lines of this layout broken at `wrap_boundaries`, for
    /// the `line_height` of its font size. A run at another size gets a line height scaled
    /// by its size, as a unitless CSS `line-height` does; an inline box gets its own extent.
    /// The runs of a line share a baseline, and the line is as tall as they need.
    pub fn line_boxes(
        &self,
        wrap_boundaries: &[WrapBoundary],
        line_height: Pixels,
        leading: LineLeading,
    ) -> SmallVec<[LineBox; 1]> {
        let lines = self.visual_lines(wrap_boundaries, line_height, leading);
        (0..=wrap_boundaries.len())
            .map(|ix| lines.get(ix))
            .collect()
    }

    pub(crate) fn visual_lines(
        &self,
        wrap_boundaries: &[WrapBoundary],
        line_height: Pixels,
        leading: LineLeading,
    ) -> LineBoxes {
        if !self.has_mixed_runs() {
            return LineBoxes::Uniform {
                height: line_height,
                baseline: line_baseline(line_height, self.ascent, self.descent, leading),
            };
        }
        let mut boxes = SmallVec::new();
        let mut top = px(0.);
        let mut extent: Option<(Pixels, Pixels)> = None;
        let mut close = |extent: Option<(Pixels, Pixels)>| {
            let (above, below) = extent
                .unwrap_or_else(|| line_extent(line_height, self.ascent, self.descent, leading));
            boxes.push(LineBox {
                top,
                height: above + below,
                baseline: above,
            });
            top += above + below;
        };
        let mut wraps = wrap_boundaries.iter().peekable();
        for (run_ix, run) in self.runs.iter().enumerate() {
            let (above, below) = self.run_extent(run, line_height, leading);
            for glyph_ix in 0..run.glyphs.len() {
                if wraps.peek() == Some(&&WrapBoundary { run_ix, glyph_ix }) {
                    wraps.next();
                    close(extent.take());
                }
                extent = Some(extent.map_or((above, below), |(a, b)| (a.max(above), b.max(below))));
            }
        }
        close(extent);
        LineBoxes::Mixed(boxes)
    }

    /// How far a run reaches above and below the baseline, its leading included.
    fn run_extent(
        &self,
        run: &ShapedRun,
        line_height: Pixels,
        leading: LineLeading,
    ) -> (Pixels, Pixels) {
        if run.inline_box {
            return (run.ascent, run.descent);
        }
        let scaled = |size: Pixels| {
            if self.font_size > px(0.) {
                line_height * (size / self.font_size)
            } else {
                line_height
            }
        };
        if let Some(place) = run.fallback {
            let (above, below) =
                line_extent(scaled(place.size), place.ascent, place.descent, leading);
            return (above + place.grow_above, below + place.grow_below);
        }
        line_extent(scaled(run.font_size), run.ascent, run.descent, leading)
    }

    /// Split this layout at a byte index, returning `(prefix, suffix)`.
    ///
    /// - `prefix` contains glyphs for bytes `[0, byte_index)` with original positions.
    ///   Its width equals the x-advance up to the split point.
    /// - `suffix` contains glyphs for bytes `[byte_index, len)` with positions
    ///   shifted left so the first glyph starts at x=0, and byte indices rebased to 0.
    /// - `font_size`, `ascent`, and `descent` are copied to both halves.
    pub fn split_at(&self, byte_index: usize) -> (LineLayout, LineLayout) {
        let x_offset = self.x_for_index(byte_index);

        // Partition glyph runs. A single run may contribute glyphs to both halves.
        let mut left_runs = Vec::new();
        let mut right_runs = Vec::new();

        for run in &self.runs {
            let split_pos = run.glyphs.partition_point(|g| g.index < byte_index);

            if split_pos > 0 {
                left_runs.push(run.with_glyphs(run.glyphs[..split_pos].to_vec()));
            }

            if split_pos < run.glyphs.len() {
                let right_glyphs = run.glyphs[split_pos..]
                    .iter()
                    .map(|g| ShapedGlyph {
                        id: g.id,
                        position: point(g.position.x - x_offset, g.position.y),
                        index: g.index - byte_index,
                        is_emoji: g.is_emoji,
                    })
                    .collect();
                right_runs.push(run.with_glyphs(right_glyphs));
            }
        }

        let left = LineLayout {
            font_size: self.font_size,
            width: x_offset,
            ascent: self.ascent,
            descent: self.descent,
            runs: left_runs,
            len: byte_index,
        };

        let right = LineLayout {
            font_size: self.font_size,
            width: self.width - x_offset,
            ascent: self.ascent,
            descent: self.descent,
            runs: right_runs,
            len: self.len - byte_index,
        };

        (left, right)
    }

    /// vue-native fork: where the line breaks when wrapped at `wrap_width`
    /// under `rules` (CSS Text 3 §5). A line breaks at its last soft wrap
    /// opportunity that fits ([`LineLayout::wrap_glyphs`]). A word that fits
    /// nowhere is broken where it overflows when `rules` allow that
    /// (`overflow-wrap: break-word | anywhere`, `word-break: break-word`), and
    /// otherwise overflows its line, which then breaks at the next opportunity.
    /// `hyphen_width` is the width of the hyphen a soft-hyphen break shows: a
    /// line that ends in one must fit it.
    pub(crate) fn wrap(
        &self,
        text: &str,
        wrap_width: Pixels,
        max_lines: Option<usize>,
        rules: TextWrap,
        hyphen_width: Pixels,
        scratch: &mut WrapScratch,
    ) -> Wrapping {
        let mut out = Wrapping::default();
        // The last opportunity on the current line: the glyph the next line
        // would start at, its x, where the line's content ends before it
        // (trailing spaces hang; a hyphen counts), and whether it shows a
        // hyphen.
        let mut candidate: Option<(WrapBoundary, Pixels, Pixels, bool)> = None;
        let mut last_boundary = WrapBoundary {
            run_ix: 0,
            glyph_ix: 0,
        };
        let mut last_boundary_x = px(0.);
        // The end of the current line's last glyph that is not a space.
        let mut content_end = px(0.);
        // The current line holds a word that may not break and is past the
        // edge: it breaks at the next opportunity, wherever that is.
        let mut overflowing = false;
        let mut widest = px(0.);
        let full = |out: &Wrapping| {
            max_lines.is_some_and(|max| out.boundaries.len() >= max.saturating_sub(1))
        };

        'glyphs: for WrapGlyph {
            boundary,
            prev_ch,
            ch,
            x,
            next_x,
            opportunity,
        } in self.wrap_glyphs(text, rules, scratch)
        {
            if let Some(hyphen) = opportunity {
                let end = if hyphen {
                    content_end + hyphen_width
                } else {
                    content_end
                };
                if overflowing && boundary > last_boundary {
                    // The overflowing word ends here: so does its line.
                    if full(&out) {
                        break 'glyphs;
                    }
                    widest = widest.max(end - last_boundary_x);
                    out.push(boundary, hyphen);
                    last_boundary = boundary;
                    last_boundary_x = x;
                    overflowing = false;
                    candidate = None;
                } else if !hyphen || end - last_boundary_x <= wrap_width {
                    candidate = Some((boundary, x, end, hyphen));
                }
            }

            // A space that crosses the edge hangs (CSS Text 3 §4.1.3): the
            // line breaks before the next word, not before the space, which
            // would start the next line with it.
            while !overflowing
                && next_x - last_boundary_x > wrap_width
                && boundary > last_boundary
                && ch != ' '
            {
                // When used line_clamp, we should limit the number of lines.
                if full(&out) {
                    break 'glyphs;
                }
                if let Some((at, at_x, end, hyphen)) = candidate.take() {
                    widest = widest.max(end - last_boundary_x);
                    out.push(at, hyphen);
                    last_boundary = at;
                    last_boundary_x = at_x;
                    // The rest of the word may still not fit on the new
                    // line: looked at again.
                } else if rules.breaks_words() && !continues_cluster(prev_ch, ch) {
                    widest = widest.max(x - last_boundary_x);
                    out.push(boundary, false);
                    last_boundary = boundary;
                    last_boundary_x = x;
                } else {
                    overflowing = true;
                }
            }
            if ch != ' ' {
                content_end = next_x;
            }
        }
        widest = widest.max(content_end - last_boundary_x);
        if widest > wrap_width {
            out.overflow = widest;
        }
        out
    }

    /// The line's min-content width (CSS Sizing 3 §5.1): its widest stretch
    /// between two of the soft wrap opportunities [`Self::wrap`] breaks at,
    /// without the spaces that end it, which hang. Under `overflow-wrap:
    /// anywhere` (and `word-break: break-word`) a break inside a word counts
    /// too, so the widest stretch is one character. Wrapped at this width,
    /// the line breaks at every opportunity it counted.
    pub(crate) fn min_content_width(
        &self,
        text: &str,
        rules: TextWrap,
        hyphen_width: Pixels,
    ) -> Pixels {
        let mut widest = px(0.);
        let mut start_x = px(0.);
        let mut end_x = px(0.);
        let mut seen_non_whitespace = false;
        let mut scratch = WrapScratch::default();
        for WrapGlyph {
            prev_ch,
            ch,
            x,
            next_x,
            opportunity,
            ..
        } in self.wrap_glyphs(text, rules, &mut scratch)
        {
            let opportunity = opportunity.or_else(|| {
                (rules.min_content_breaks_words()
                    && seen_non_whitespace
                    && ch != ' '
                    && !continues_cluster(prev_ch, ch))
                .then_some(false)
            });
            if let Some(hyphen) = opportunity {
                let hyphen = if hyphen { hyphen_width } else { px(0.) };
                widest = widest.max(end_x - start_x + hyphen);
                start_x = x;
            }
            if ch != ' ' {
                seen_non_whitespace = true;
                end_x = next_x;
            }
        }
        widest.max(end_x - start_x)
    }

    /// vue-native fork: the line's glyphs in visual order, one per shaped
    /// cluster, newlines skipped, each with the soft wrap opportunity before
    /// it under `rules`.
    ///
    /// The opportunities are UAX #14's ([`css_break_indices`], tailored by
    /// `word-break` and `hyphens`), less those inside a shaped cluster.
    /// Transitions between line-break segments are detected rather than
    /// glyph indices matched directly, to accommodate RTL text (a workaround
    /// for the lack of complete UAX #9 support). The first segment that is
    /// not a space opens no opportunity: a line never breaks before its first
    /// word.
    fn wrap_glyphs<'a>(
        &'a self,
        text: &'a str,
        rules: TextWrap,
        scratch: &'a mut WrapScratch,
    ) -> impl Iterator<Item = WrapGlyph> + 'a {
        // Shaping can combine characters across a line break opportunity, so we ignore those
        // break opportunities that would separate them.
        let WrapScratch {
            glyph_indices,
            break_indices,
        } = scratch;
        glyph_indices.clear();
        glyph_indices.extend(
            self.runs
                .iter()
                .flat_map(|run| run.glyphs.iter().map(|glyph| glyph.index)),
        );
        break_indices.clear();
        break_indices
            .extend(css_break_indices(text, rules).filter(|index| glyph_indices.contains(index)));
        let break_indices = &*break_indices;
        let mut glyphs = self
            .runs
            .iter()
            .enumerate()
            .flat_map(move |(run_ix, run)| {
                run.glyphs.iter().enumerate().map(move |(glyph_ix, glyph)| {
                    (
                        WrapBoundary { run_ix, glyph_ix },
                        glyph.index,
                        glyph.position.x,
                    )
                })
            })
            .dedup_by(|(_, previous_index, ..), (_, index, ..)| previous_index == index)
            .peekable();
        let mut current_segment: Option<Range<usize>> = None;
        let mut prev_ch = '\0';
        std::iter::from_fn(move || {
            loop {
                let (boundary, index, x) = glyphs.next()?;
                let next_x = glyphs.peek().map_or(self.width, |(_, _, x)| *x);
                let ch = text[index..].chars().next().unwrap_or(' ');
                if ch == '\n' {
                    continue;
                }
                let mut opportunity = None;
                if ch != ' '
                    && current_segment
                        .as_ref()
                        .is_none_or(|segment| !segment.contains(&index))
                {
                    let segment =
                        break_indices.partition_point(|break_index| *break_index <= index);
                    let start = segment
                        .checked_sub(1)
                        .and_then(|previous| break_indices.get(previous))
                        .copied()
                        .unwrap_or(0);
                    let end = break_indices.get(segment).copied().unwrap_or(text.len());
                    if current_segment.is_some() {
                        // A break after a soft hyphen shows a hyphen; under
                        // `hyphens: none` there is no such break.
                        opportunity = Some(text[..start].ends_with('\u{AD}'));
                    }
                    current_segment = Some(start..end);
                }
                let glyph = WrapGlyph {
                    boundary,
                    prev_ch,
                    ch,
                    x,
                    next_x,
                    opportunity,
                };
                prev_ch = ch;
                return Some(glyph);
            }
        })
    }
}

/// vue-native fork: one glyph of [`LineLayout::wrap_glyphs`].
struct WrapGlyph {
    boundary: WrapBoundary,
    /// The character of the glyph before, `'\0'` for the first.
    prev_ch: char,
    ch: char,
    x: Pixels,
    /// Where the next glyph starts, or the line's width after the last.
    next_x: Pixels,
    /// A soft wrap opportunity before this glyph: `Some(true)` when the
    /// break shows a hyphen.
    opportunity: Option<bool>,
}

/// vue-native fork: what [`LineLayout::wrap`] decided for one line of text.
#[derive(Clone, Debug, Default)]
pub(crate) struct Wrapping {
    pub(crate) boundaries: SmallVec<[WrapBoundary; 1]>,
    /// The boundaries a soft hyphen made, as indices into `boundaries`: the
    /// line before each one ends with a visible hyphen.
    pub(crate) hyphens: SmallVec<[usize; 1]>,
    /// The widest line, trailing spaces not counted, when one is wider than
    /// the wrap width (a word that may not break). Zero otherwise.
    pub(crate) overflow: Pixels,
}

impl Wrapping {
    fn push(&mut self, at: WrapBoundary, hyphen: bool) {
        if hyphen {
            self.hyphens.push(self.boundaries.len());
        }
        self.boundaries.push(at);
    }
}

/// vue-native fork: the byte offsets in `text` a line may break before under
/// `rules`: UAX #14's opportunities ([`LineWrapper::break_indices`]), tailored
/// by CSS Text 3:
/// - `word-break: break-all`: between any two letters, though not before
///   punctuation that never starts a line; `keep-all`: no break between two
///   CJK letters (UAX #14's own tailorings, the same as CSS's);
/// - after a soft hyphen (U+00AD) only under `hyphens: manual`;
/// - after a zero width space (U+200B), as UAX #14 has it.
///
/// No opportunity splits a grapheme cluster.
fn css_break_indices(text: &str, rules: TextWrap) -> impl Iterator<Item = usize> + '_ {
    let word_option = match rules.word_break {
        WordBreak::BreakAll => Some(LineBreakWordOption::BreakAll),
        WordBreak::KeepAll => Some(LineBreakWordOption::KeepAll),
        WordBreak::Normal | WordBreak::BreakWord => None,
    };
    let indices: Box<dyn Iterator<Item = usize> + '_> = match word_option {
        None => Box::new(LineWrapper::break_indices(text)),
        Some(word_option) => {
            let mut options = LineBreakOptions::default();
            options.word_option = Some(word_option);
            Box::new(LineSegmenter::new_for_non_complex_scripts(options).segment_str(text))
        }
    };
    indices.filter(move |index| {
        rules.hyphens == Hyphens::Manual || !text[..*index].ends_with('\u{AD}')
    })
}

/// Does `ch` belong to the grapheme cluster `prev` is in? A combining mark,
/// a joiner, a variation selector, an emoji modifier or tag, or anything
/// after a zero width joiner. A shaper usually gives a cluster one glyph, but
/// not always; a break between two of its glyphs would split a character.
/// Used where a line breaks inside a word (`overflow-wrap`), which UAX #14's
/// opportunities say nothing about.
fn continues_cluster(prev: char, ch: char) -> bool {
    prev == '\u{200D}'
        || matches!(
            ch,
            '\u{0300}'..='\u{036F}'
                | '\u{0483}'..='\u{0489}'
                | '\u{0591}'..='\u{05BD}'
                | '\u{0610}'..='\u{061A}'
                | '\u{064B}'..='\u{065F}'
                | '\u{0900}'..='\u{0903}'
                | '\u{093A}'..='\u{094F}'
                | '\u{1AB0}'..='\u{1AFF}'
                | '\u{1DC0}'..='\u{1DFF}'
                | '\u{200C}'..='\u{200D}'
                | '\u{20D0}'..='\u{20FF}'
                | '\u{FE00}'..='\u{FE0F}'
                | '\u{FE20}'..='\u{FE2F}'
                | '\u{1F3FB}'..='\u{1F3FF}'
                | '\u{E0020}'..='\u{E007F}'
                | '\u{E0100}'..='\u{E01EF}'
        )
}

/// A line of text that has been wrapped to fit a given width
#[derive(Default, Debug)]
pub struct WrappedLineLayout {
    /// The line layout, pre-wrapping.
    pub unwrapped_layout: Arc<LineLayout>,

    /// The boundaries at which the line was wrapped
    pub wrap_boundaries: SmallVec<[WrapBoundary; 1]>,

    /// The width of the line, if it was wrapped
    pub wrap_width: Option<Pixels>,

    /// vue-native fork: the wrap boundaries a soft hyphen made, as indices
    /// into `wrap_boundaries`. The line before each ends with a hyphen.
    pub(crate) hyphen_breaks: SmallVec<[usize; 1]>,

    /// vue-native fork: the widest line, when a word that may not break made
    /// it wider than the wrap width. Zero otherwise.
    pub(crate) overflow_width: Pixels,

    /// vue-native fork: CSS `line-leading` of the text.
    pub(crate) leading: LineLeading,
}

/// A boundary at which a line was wrapped
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct WrapBoundary {
    /// The index in the run just before the line was wrapped
    pub run_ix: usize,
    /// The index of the glyph just before the line was wrapped
    pub glyph_ix: usize,
}

impl WrappedLineLayout {
    /// The length of the underlying text, in utf8 bytes.
    pub fn len(&self) -> usize {
        self.unwrapped_layout.len
    }

    /// The width of this line, in pixels, whether or not it was wrapped.
    pub fn width(&self) -> Pixels {
        self.wrap_width
            .unwrap_or(Pixels::MAX)
            .min(self.unwrapped_layout.width)
            // A line that overflows the wrap width is as wide as it is.
            .max(self.overflow_width)
    }

    /// The size of the whole wrapped text, for the given line_height.
    /// can span multiple lines if there are multiple wrap boundaries.
    pub fn size(&self, line_height: Pixels) -> Size<Pixels> {
        Size {
            width: self.width(),
            height: self
                .visual_lines(line_height)
                .total_height(self.wrap_boundaries.len() + 1),
        }
    }

    /// vue-native fork: where each visual line sits; see [`LineLayout::line_boxes`].
    pub fn line_boxes(&self, line_height: Pixels) -> SmallVec<[LineBox; 1]> {
        self.unwrapped_layout
            .line_boxes(&self.wrap_boundaries, line_height, self.leading)
    }

    pub(crate) fn visual_lines(&self, line_height: Pixels) -> LineBoxes {
        self.unwrapped_layout
            .visual_lines(&self.wrap_boundaries, line_height, self.leading)
    }

    /// The ascent of a line in this layout
    pub fn ascent(&self) -> Pixels {
        self.unwrapped_layout.ascent
    }

    /// The descent of a line in this layout
    pub fn descent(&self) -> Pixels {
        self.unwrapped_layout.descent
    }

    /// The wrap boundaries in this layout
    pub fn wrap_boundaries(&self) -> &[WrapBoundary] {
        &self.wrap_boundaries
    }

    /// The font size of this layout
    pub fn font_size(&self) -> Pixels {
        self.unwrapped_layout.font_size
    }

    /// The runs in this layout, sans wrapping
    pub fn runs(&self) -> &[ShapedRun] {
        &self.unwrapped_layout.runs
    }

    /// The index corresponding to a given position in this layout for the given line height.
    ///
    /// See also [`Self::closest_index_for_position`].
    pub fn index_for_position(
        &self,
        position: Point<Pixels>,
        line_height: Pixels,
    ) -> Result<usize, usize> {
        self._index_for_position(position, line_height, false)
    }

    /// The closest index to a given position in this layout for the given line height.
    ///
    /// Closest means the character boundary closest to the given position.
    ///
    /// See also [`LineLayout::closest_index_for_x`].
    pub fn closest_index_for_position(
        &self,
        position: Point<Pixels>,
        line_height: Pixels,
    ) -> Result<usize, usize> {
        self._index_for_position(position, line_height, true)
    }

    fn _index_for_position(
        &self,
        position: Point<Pixels>,
        line_height: Pixels,
        closest: bool,
    ) -> Result<usize, usize> {
        let wrapped_line_ix = self.visual_lines(line_height).line_at(position.y);

        let wrapped_line_start_index;
        let wrapped_line_start_x;
        if wrapped_line_ix > 0 {
            let Some(line_start_boundary) = self.wrap_boundaries.get(wrapped_line_ix - 1) else {
                return Err(0);
            };
            let run = &self.unwrapped_layout.runs[line_start_boundary.run_ix];
            let glyph = &run.glyphs[line_start_boundary.glyph_ix];
            wrapped_line_start_index = glyph.index;
            wrapped_line_start_x = glyph.position.x;
        } else {
            wrapped_line_start_index = 0;
            wrapped_line_start_x = Pixels::ZERO;
        };

        let wrapped_line_end_index;
        let wrapped_line_end_x;
        if wrapped_line_ix < self.wrap_boundaries.len() {
            let next_wrap_boundary_ix = wrapped_line_ix;
            let next_wrap_boundary = self.wrap_boundaries[next_wrap_boundary_ix];
            let run = &self.unwrapped_layout.runs[next_wrap_boundary.run_ix];
            let glyph = &run.glyphs[next_wrap_boundary.glyph_ix];
            wrapped_line_end_index = glyph.index;
            wrapped_line_end_x = glyph.position.x;
        } else {
            wrapped_line_end_index = self.unwrapped_layout.len;
            wrapped_line_end_x = self.unwrapped_layout.width;
        };

        let mut position_in_unwrapped_line = position;
        position_in_unwrapped_line.x += wrapped_line_start_x;
        if position_in_unwrapped_line.x < wrapped_line_start_x {
            Err(wrapped_line_start_index)
        } else if position_in_unwrapped_line.x >= wrapped_line_end_x {
            Err(wrapped_line_end_index)
        } else {
            if closest {
                Ok(self
                    .unwrapped_layout
                    .closest_index_for_x(position_in_unwrapped_line.x))
            } else {
                // The shaper can place a trailing zero-width wrap boundary glyph slightly past
                // the line's width, so the row can extend past where `index_for_x` has glyphs.
                self.unwrapped_layout
                    .index_for_x(position_in_unwrapped_line.x)
                    .ok_or(wrapped_line_end_index)
            }
        }
    }

    /// Returns the pixel position for the given byte index.
    pub fn position_for_index(&self, index: usize, line_height: Pixels) -> Option<Point<Pixels>> {
        let lines = self.visual_lines(line_height);
        let mut line_start_ix = 0;
        let line_end_indices = self
            .wrap_boundaries
            .iter()
            .map(|wrap_boundary| {
                let run = &self.unwrapped_layout.runs[wrap_boundary.run_ix];
                let glyph = &run.glyphs[wrap_boundary.glyph_ix];
                glyph.index
            })
            .chain([self.len()])
            .enumerate();
        for (ix, line_end_ix) in line_end_indices {
            let line_y = lines.get(ix).top;
            if index < line_start_ix {
                break;
            } else if index > line_end_ix {
                line_start_ix = line_end_ix;
                continue;
            } else {
                let line_start_x = self.unwrapped_layout.x_for_index(line_start_ix);
                let x = self.unwrapped_layout.x_for_index(index) - line_start_x;
                return Some(point(x, line_y));
            }
        }

        None
    }
}

/// Moves the layouts in `unused` that are held outside the cache into `cache`, and drops
/// the rest. The two never share a key, so only the smaller map's entries are moved.
fn keep_held_layouts<V>(
    cache: &mut FxHashMap<Arc<CacheKey>, Arc<V>>,
    unused: &mut FxHashMap<Arc<CacheKey>, Arc<V>>,
) {
    unused.retain(|_, layout| Arc::strong_count(layout) > 1);
    if unused.len() > cache.len() {
        std::mem::swap(cache, unused);
    }
    cache.extend(unused.drain());
}

pub(crate) struct LineLayoutCache {
    state: RefCell<LineLayoutCacheState>,
    wrap_scratch: RefCell<WrapScratch>,
    text_system: Arc<TextSystem>,
    /// vue-native fork: [`Self::asked_extent`] by font and size.
    asked_extents: RefCell<FxHashMap<(FontId, u32), (Pixels, Pixels)>>,
    /// Advances when [`TextSystem::add_fonts`] successfully changes the font database.
    font_generation: Arc<AtomicUsize>,
    /// Records the generation represented by both frame caches.
    cached_font_generation: AtomicUsize,
}

#[derive(Default)]
pub(crate) struct WrapScratch {
    glyph_indices: FxHashSet<usize>,
    break_indices: Vec<usize>,
}

#[derive(Default)]
struct LineLayoutCacheState {
    previous_frame: FrameCache,
    current_frame: FrameCache,
}

#[derive(Default)]
struct FrameCache {
    lines: FxHashMap<Arc<CacheKey>, Arc<LineLayout>>,
    wrapped_lines: FxHashMap<Arc<CacheKey>, Arc<WrappedLineLayout>>,
    used_lines: Vec<Arc<CacheKey>>,
    used_wrapped_lines: Vec<Arc<CacheKey>>,
}

#[derive(Clone, Default)]
pub(crate) struct LineLayoutIndex {
    font_generation: usize,
    lines_index: usize,
    wrapped_lines_index: usize,
}

impl LineLayoutCache {
    pub fn new(text_system: Arc<TextSystem>, font_generation: Arc<AtomicUsize>) -> Self {
        let cached_font_generation = font_generation.load(Ordering::Acquire);
        Self {
            state: RefCell::default(),
            wrap_scratch: RefCell::default(),
            text_system,
            asked_extents: RefCell::default(),
            font_generation,
            cached_font_generation: AtomicUsize::new(cached_font_generation),
        }
    }

    pub fn layout_index(&self) -> LineLayoutIndex {
        let font_generation = self.clear_if_font_generation_changed();
        let state = self.state.borrow();
        let frame = &state.current_frame;
        LineLayoutIndex {
            font_generation,
            lines_index: frame.used_lines.len(),
            wrapped_lines_index: frame.used_wrapped_lines.len(),
        }
    }

    pub fn reuse_layouts(&self, range: Range<LineLayoutIndex>) {
        let font_generation = self.clear_if_font_generation_changed();
        if range.start.font_generation != font_generation
            || range.end.font_generation != font_generation
        {
            return;
        }
        let mut state = self.state.borrow_mut();
        let LineLayoutCacheState {
            previous_frame,
            current_frame,
        } = &mut *state;

        for key in &previous_frame.used_lines[range.start.lines_index..range.end.lines_index] {
            if let Some((key, line)) = previous_frame.lines.remove_entry(key) {
                current_frame.lines.insert(key, line);
            }
            current_frame.used_lines.push(key.clone());
        }

        for key in &previous_frame.used_wrapped_lines
            [range.start.wrapped_lines_index..range.end.wrapped_lines_index]
        {
            if let Some((key, line)) = previous_frame.wrapped_lines.remove_entry(key) {
                current_frame.wrapped_lines.insert(key, line);
            }
            current_frame.used_wrapped_lines.push(key.clone());
        }
    }

    pub fn truncate_layouts(&self, index: LineLayoutIndex) {
        let font_generation = self.clear_if_font_generation_changed();
        if index.font_generation != font_generation {
            return;
        }
        let mut state = self.state.borrow_mut();
        let current_frame = &mut state.current_frame;
        current_frame.used_lines.truncate(index.lines_index);
        current_frame
            .used_wrapped_lines
            .truncate(index.wrapped_lines_index);
    }

    pub fn finish_frame(&self) {
        let _font_generation = self.clear_if_font_generation_changed();
        let LineLayoutCacheState {
            previous_frame,
            current_frame,
        } = &mut *self.state.borrow_mut();
        std::mem::swap(previous_frame, current_frame);
        // Text that kept its measurement isn't shaped again, so the layouts it holds must
        // stay cached. Wrapped lines go first, since they hold their unwrapped lines.
        keep_held_layouts(
            &mut previous_frame.wrapped_lines,
            &mut current_frame.wrapped_lines,
        );
        keep_held_layouts(&mut previous_frame.lines, &mut current_frame.lines);
        current_frame.used_lines.clear();
        current_frame.used_wrapped_lines.clear();
    }

    /// vue-native fork: the width of the hyphen a soft-hyphen break shows,
    /// in the first run's font. Zero when no break can show one.
    fn hyphen_width(
        &self,
        text: &str,
        font_size: Pixels,
        runs: &[FontRun],
        rules: TextWrap,
    ) -> Pixels {
        if rules.hyphens != Hyphens::Manual || !text.contains('\u{AD}') {
            return px(0.);
        }
        let Some(font_id) = runs.first().map(|run| run.font_id) else {
            return px(0.);
        };
        let Some(glyph) = self
            .text_system
            .platform_text_system
            .glyph_for_char(font_id, '-')
        else {
            return px(0.);
        };
        let Ok(advance) = self
            .text_system
            .platform_text_system
            .advance(font_id, glyph)
        else {
            return px(0.);
        };
        let units_per_em = self
            .text_system
            .platform_text_system
            .font_metrics(font_id)
            .units_per_em as f32;
        px(advance.width / units_per_em * font_size.0)
    }

    /// vue-native fork: CSS `tab-size`. Each tab advances to the next stop, measured from the
    /// start of the line; a stop closer than half a space is skipped (CSS Text 3 §4.2). A
    /// `Spaces` stop is in the tab's own font, size and spacing.
    fn apply_tab_stops(&self, layout: &mut LineLayout, text: &str, runs: &[FontRun], tab: TabSize) {
        let run_at = |offset: usize| {
            let mut start = 0;
            for run in runs {
                if offset < start + run.len {
                    return Some(*run);
                }
                start += run.len;
            }
            runs.last().copied()
        };
        let space = |font_id: FontId, size: Pixels, offset: usize| {
            let run = run_at(offset);
            let shaped = self.text_system.platform_text_system.layout_line(
                " ",
                size,
                &[FontRun {
                    len: 1,
                    font_id,
                    tracking: px(0.),
                    word_spacing: px(0.),
                }],
            );
            shaped.width + run.map_or(px(0.), |r| r.tracking + r.word_spacing)
        };

        let starts: Vec<(usize, usize, Pixels)> = layout
            .runs
            .iter()
            .enumerate()
            .flat_map(|(r, run)| {
                run.glyphs
                    .iter()
                    .enumerate()
                    .map(move |(g, glyph)| (r, g, glyph.position.x))
            })
            .collect();
        let width = layout.width;
        let mut shift = px(0.);
        let mut previous_index = None;
        for (k, &(r, g, x)) in starts.iter().enumerate() {
            let index = layout.runs[r].glyphs[g].index;
            layout.runs[r].glyphs[g].position.x += shift;
            if previous_index == Some(index) || !text[index..].starts_with('\t') {
                previous_index = Some(index);
                continue;
            }
            previous_index = Some(index);
            let next_x = starts[k + 1..]
                .iter()
                .find(|&&(nr, ng, _)| layout.runs[nr].glyphs[ng].index != index)
                .map_or(width, |&(_, _, nx)| nx);
            let natural = next_x - x;
            let run = &layout.runs[r];
            let space_width = space(run.font_id, run.font_size.max(px(1.)), index);
            let interval = match tab {
                TabSize::Spaces(n) => space_width * n,
                TabSize::Length(l) => l,
            };
            let start = x + shift;
            let advance = if interval <= px(0.) {
                px(0.)
            } else {
                let mut stop = interval * ((start / interval).floor() + 1.);
                if stop - start < space_width * 0.5 {
                    stop += interval;
                }
                stop - start
            };
            shift += advance - natural;
        }
        layout.width += shift;
    }

    #[cfg(test)]
    pub(crate) fn last_frame_wrapped_line_count(&self) -> usize {
        self.state.borrow().previous_frame.used_wrapped_lines.len()
    }

    pub fn layout_wrapped_line<Text>(
        &self,
        text: Text,
        font_size: Pixels,
        runs: &[FontRun],
        spans: &[LayoutSpan],
        wrap_width: Option<Pixels>,
        max_lines: Option<usize>,
        rules: TextWrap,
    ) -> Arc<WrappedLineLayout>
    where
        Text: AsRef<str>,
        SharedString: From<Text>,
    {
        let _font_generation = self.clear_if_font_generation_changed();
        let key = &CacheKeyRef {
            text: text.as_ref(),
            font_size,
            runs,
            spans,
            wrap_width,
            max_lines,
            force_width: None,
            wrap: rules,
        } as &dyn AsCacheKeyRef;

        let state = self.state.borrow();
        if let Some(layout) = state.current_frame.wrapped_lines.get(key) {
            return layout.clone();
        }
        drop(state);

        let mut state = self.state.borrow_mut();
        let previous_frame_entry = state.previous_frame.wrapped_lines.remove_entry(key);
        if let Some((key, layout)) = previous_frame_entry {
            state
                .current_frame
                .wrapped_lines
                .insert(key.clone(), layout.clone());
            state.current_frame.used_wrapped_lines.push(key);
            layout
        } else {
            drop(state);
            let text = SharedString::from(text);
            let unwrapped_layout =
                self.layout_line_with::<&SharedString>(&text, font_size, runs, spans, None, rules);
            let wrapping = if let Some(wrap_width) = wrap_width {
                let hyphen_width = self.hyphen_width(&text, font_size, runs, rules);
                unwrapped_layout.wrap(
                    text.as_ref(),
                    wrap_width,
                    max_lines,
                    rules,
                    hyphen_width,
                    &mut self.wrap_scratch.borrow_mut(),
                )
            } else {
                Wrapping::default()
            };
            let layout = Arc::new(WrappedLineLayout {
                unwrapped_layout,
                wrap_boundaries: wrapping.boundaries,
                wrap_width,
                hyphen_breaks: wrapping.hyphens,
                overflow_width: wrapping.overflow,
                leading: rules.line_box.leading,
            });
            let key = Arc::new(CacheKey {
                text,
                font_size,
                runs: SmallVec::from(runs),
                spans: SmallVec::from(spans),
                wrap_width,
                max_lines,
                force_width: None,
                wrap: rules,
            });

            let mut state = self.state.borrow_mut();
            state
                .current_frame
                .wrapped_lines
                .insert(key.clone(), layout.clone());
            state.current_frame.used_wrapped_lines.push(key);

            layout
        }
    }

    pub fn layout_line<Text>(
        &self,
        text: Text,
        font_size: Pixels,
        runs: &[FontRun],
        spans: &[LayoutSpan],
        force_width: Option<Pixels>,
    ) -> Arc<LineLayout>
    where
        Text: AsRef<str>,
        SharedString: From<Text>,
    {
        self.layout_line_with(
            text,
            font_size,
            runs,
            spans,
            force_width,
            TextWrap::default(),
        )
    }

    /// vue-native fork: [`Self::layout_line`] under `rules`: tabs advancing to stops `tab_size`
    /// apart, each run's box under `line_box`, fallback fonts sized by `font_size_adjust`. The
    /// breaking rules are not read.
    pub fn layout_line_with<Text>(
        &self,
        text: Text,
        font_size: Pixels,
        runs: &[FontRun],
        spans: &[LayoutSpan],
        force_width: Option<Pixels>,
        rules: TextWrap,
    ) -> Arc<LineLayout>
    where
        Text: AsRef<str>,
        SharedString: From<Text>,
    {
        let _font_generation = self.clear_if_font_generation_changed();
        let wrap = TextWrap {
            tab_size: rules.tab_size,
            line_box: rules.line_box,
            font_size_adjust: rules.font_size_adjust,
            ..TextWrap::default()
        };
        let (tab_size, metrics) = (rules.tab_size, rules.line_box.metrics);
        let key = &CacheKeyRef {
            text: text.as_ref(),
            font_size,
            runs,
            spans,
            wrap_width: None,
            max_lines: None,
            force_width,
            wrap,
        } as &dyn AsCacheKeyRef;

        count_lookup();
        let state = self.state.borrow();
        if let Some(layout) = state.current_frame.lines.get(key) {
            return layout.clone();
        }
        drop(state);

        let mut state = self.state.borrow_mut();
        let LineLayoutCacheState {
            previous_frame,
            current_frame,
        } = &mut *state;
        if let Some((key, layout)) = previous_frame.lines.remove_entry(key) {
            current_frame.lines.insert(key.clone(), layout.clone());
            current_frame.used_lines.push(key);
            layout
        } else {
            let text = SharedString::from(text);
            let shaping = std::time::Instant::now();
            let mut layout = if spans.is_empty() {
                let mut layout = self
                    .text_system
                    .platform_text_system
                    .layout_line(&text, font_size, runs);
                for run in &mut layout.runs {
                    run.font_size = font_size;
                    run.ascent = layout.ascent;
                    run.descent = layout.descent;
                }
                layout
            } else {
                self.layout_spans(&text, font_size, runs, spans)
            };
            if let Some(aspect) = rules.font_size_adjust
                && let Some(adjusted) =
                    self.adjust_fallback_sizes(&text, font_size, runs, spans, &layout, aspect)
            {
                layout = adjusted;
            }
            count_shape(shaping);

            // Spacing first: `force_width` assigns each cluster an ABSOLUTE
            // cell, so it has to see the positions it is going to overwrite,
            // not ones already widened by tracking.
            apply_spacing_to_layout(&mut layout, &text, runs);
            if text.contains('\t') {
                self.apply_tab_stops(&mut layout, &text, runs, tab_size);
            }
            if let Some(force_width) = force_width {
                apply_force_width_to_layout(&mut layout, force_width);
            }
            if metrics == LineMetrics::Snapped {
                self.snap_extents(&mut layout, font_size, runs);
            }
            self.place_fallback_runs(&mut layout, font_size, runs, spans, rules.line_box);

            let key = Arc::new(CacheKey {
                text,
                font_size,
                runs: SmallVec::from(runs),
                spans: SmallVec::from(spans),
                wrap_width: None,
                max_lines: None,
                force_width,
                wrap,
            });
            let layout = Arc::new(layout);
            current_frame.lines.insert(key.clone(), layout.clone());
            current_frame.used_lines.push(key);
            layout
        }
    }

    /// vue-native fork: a line with stretches at other font sizes, or inline boxes. Each
    /// stretch of text is shaped on its own, at its size, and the stretches are laid end to
    /// end on one baseline: shaping does not carry across a change of size, as in browsers.
    fn layout_spans(
        &self,
        text: &str,
        font_size: Pixels,
        runs: &[FontRun],
        spans: &[LayoutSpan],
    ) -> LineLayout {
        let mut layout = LineLayout {
            font_size,
            len: text.len(),
            ..Default::default()
        };
        let mut runs = runs.iter().copied();
        let mut pending: Option<FontRun> = None;
        let mut segment_runs = SmallVec::<[FontRun; 4]>::new();
        let mut start = 0;
        for span in spans {
            let end = (start + span.len).min(text.len());
            segment_runs.clear();
            let mut needed = end - start;
            while needed > 0 {
                let Some(run) = pending.take().or_else(|| runs.next()) else {
                    break;
                };
                let len = run.len.min(needed);
                segment_runs.push(FontRun { len, ..run });
                if run.len > len {
                    pending = Some(FontRun {
                        len: run.len - len,
                        ..run
                    });
                }
                needed -= len;
            }
            if let Some(inline_box) = span.inline_box {
                let font_id = segment_runs.first().map_or(FontId(0), |run| run.font_id);
                layout.runs.push(ShapedRun {
                    font_id,
                    font_size: span.font_size,
                    ascent: inline_box.ascent,
                    descent: inline_box.descent,
                    inline_box: true,
                    fallback: None,
                    glyphs: vec![ShapedGlyph {
                        id: GlyphId(0),
                        position: point(layout.width, px(0.)),
                        index: start,
                        is_emoji: false,
                    }],
                });
                layout.width += inline_box.width;
                layout.ascent = layout.ascent.max(inline_box.ascent);
                layout.descent = layout.descent.max(inline_box.descent);
            } else if end > start {
                let segment = self.text_system.platform_text_system.layout_line(
                    &text[start..end],
                    span.font_size,
                    &segment_runs,
                );
                for mut run in segment.runs {
                    for glyph in &mut run.glyphs {
                        glyph.index += start;
                        glyph.position.x += layout.width;
                    }
                    run.font_size = span.font_size;
                    run.ascent = segment.ascent;
                    run.descent = segment.descent;
                    layout.runs.push(run);
                }
                layout.width += segment.width;
                layout.ascent = layout.ascent.max(segment.ascent);
                layout.descent = layout.descent.max(segment.descent);
            }
            start = end;
        }
        layout
    }

    /// vue-native fork: each text run's box from its own font under `line-metrics: snapped`, and
    /// the line's as the largest above and below the baseline. An empty line takes its first
    /// run's font.
    fn snap_extents(&self, layout: &mut LineLayout, font_size: Pixels, runs: &[FontRun]) {
        let (mut ascent, mut descent) = (px(0.), px(0.));
        let mut text_runs = 0;
        for run in &mut layout.runs {
            if !run.inline_box {
                (run.ascent, run.descent) =
                    self.text_system
                        .line_extent(run.font_id, run.font_size, LineMetrics::Snapped);
                text_runs += 1;
            }
            ascent = ascent.max(run.ascent);
            descent = descent.max(run.descent);
        }
        if text_runs == 0
            && let Some(run) = runs.first()
        {
            let (a, d) = self
                .text_system
                .line_extent(run.font_id, font_size, LineMetrics::Snapped);
            ascent = ascent.max(a);
            descent = descent.max(d);
        }
        layout.ascent = ascent;
        layout.descent = descent;
    }

    /// vue-native fork: `layout` shaped again with each stretch a fallback font draws at the size
    /// that gives the font the asked font's `aspect` (CSS `font-size-adjust`). `None` when no
    /// stretch changes size.
    fn adjust_fallback_sizes(
        &self,
        text: &str,
        font_size: Pixels,
        runs: &[FontRun],
        spans: &[LayoutSpan],
        layout: &LineLayout,
        aspect: FontAspect,
    ) -> Option<LineLayout> {
        let mut factors: SmallVec<[(usize, f32); 8]> = SmallVec::new();
        for run in &layout.runs {
            let Some(start) = run.glyphs.iter().map(|glyph| glyph.index).min() else {
                continue;
            };
            let asked = asked_font(runs, start);
            let factor = if run.inline_box || run.font_id == asked {
                1.
            } else {
                let own = self.text_system.font_aspect(run.font_id, aspect);
                let wanted = self.text_system.font_aspect(asked, aspect);
                if own > 0. && wanted > 0. {
                    wanted / own
                } else {
                    1.
                }
            };
            factors.push((start, factor));
        }
        if factors
            .iter()
            .all(|&(_, factor)| (factor - 1.).abs() < 1e-3)
        {
            return None;
        }
        factors.sort_by_key(|&(start, _)| start);

        let whole = [LayoutSpan {
            len: text.len(),
            font_size,
            inline_box: None,
        }];
        let base = if spans.is_empty() { &whole[..] } else { spans };
        let mut adjusted: Vec<LayoutSpan> = Vec::with_capacity(base.len() + factors.len());
        let mut span_start = 0;
        for span in base {
            let span_end = (span_start + span.len).min(text.len());
            if span.inline_box.is_some() {
                adjusted.push(*span);
                span_start = span_end;
                continue;
            }
            let mut at = span_start;
            while at < span_end {
                let next = factors.partition_point(|&(start, _)| start <= at);
                let factor = next.checked_sub(1).map_or(1., |ix| factors[ix].1);
                let end = factors
                    .get(next)
                    .map_or(span_end, |&(start, _)| start.min(span_end));
                let size = span.font_size * factor;
                match adjusted.last_mut() {
                    Some(last) if last.inline_box.is_none() && last.font_size == size => {
                        last.len += end - at;
                    }
                    _ => adjusted.push(LayoutSpan {
                        len: end - at,
                        font_size: size,
                        inline_box: None,
                    }),
                }
                at = end;
            }
            span_start = span_end;
        }
        Some(self.layout_spans(text, font_size, runs, &adjusted))
    }

    /// vue-native fork: a [`FallbackPlace`] for each run drawn with a font its text did not ask
    /// for, where that changes the line: under `line-height: normal` when the font's box reaches
    /// past the asked font's, and when `font-size-adjust` shaped it at another size. The other text
    /// runs of such a line take the asked font's own extent, not the one the platform reports for
    /// the whole line, which already counts the fallback font.
    fn place_fallback_runs(
        &self,
        layout: &mut LineLayout,
        font_size: Pixels,
        runs: &[FontRun],
        spans: &[LayoutSpan],
        line_box: LineBoxRules,
    ) {
        let metrics = line_box.metrics;
        let mut placed = false;
        for ix in 0..layout.runs.len() {
            let run = &layout.runs[ix];
            if run.inline_box {
                continue;
            }
            let Some(start) = run.glyphs.iter().map(|glyph| glyph.index).min() else {
                continue;
            };
            let asked = asked_font(runs, start);
            let size = asked_size(spans, font_size, start);
            if run.font_id == asked && run.font_size == size {
                continue;
            }
            let (mut grow_above, mut grow_below) = (px(0.), px(0.));
            if let Some(line_height) = line_box.normal
                && run.font_id != asked
            {
                let line_height = if font_size > px(0.) {
                    line_height * (size / font_size)
                } else {
                    line_height
                };
                let (own_above, own_below) =
                    self.text_system
                        .normal_line_extent(run.font_id, run.font_size, metrics);
                let (_, asked_below) = self.text_system.normal_line_extent(asked, size, metrics);
                grow_above = (own_above - (line_height - asked_below)).max(px(0.));
                grow_below = (own_below - asked_below).max(px(0.));
            }
            if grow_above == px(0.) && grow_below == px(0.) && run.font_size == size {
                continue;
            }
            let (ascent, descent) = self.asked_extent(asked, size, metrics);
            layout.runs[ix].fallback = Some(FallbackPlace {
                size,
                ascent,
                descent,
                grow_above,
                grow_below,
            });
            placed = true;
        }
        if !placed || metrics == LineMetrics::Snapped {
            return;
        }
        for ix in 0..layout.runs.len() {
            let run = &layout.runs[ix];
            if run.inline_box || run.fallback.is_some() {
                continue;
            }
            let Some(start) = run.glyphs.iter().map(|glyph| glyph.index).min() else {
                continue;
            };
            let (ascent, descent) =
                self.asked_extent(asked_font(runs, start), run.font_size, metrics);
            let run = &mut layout.runs[ix];
            (run.ascent, run.descent) = (ascent, descent);
        }
    }

    /// vue-native fork: how far a line of `font_id` alone reaches above and below the baseline,
    /// as the platform lays it out (`snapped`: the font's snapped box).
    fn asked_extent(
        &self,
        font_id: FontId,
        font_size: Pixels,
        metrics: LineMetrics,
    ) -> (Pixels, Pixels) {
        if metrics == LineMetrics::Snapped {
            return self.text_system.line_extent(font_id, font_size, metrics);
        }
        let key = (font_id, f32::from(font_size).to_bits());
        if let Some(extent) = self.asked_extents.borrow().get(&key) {
            return *extent;
        }
        let line = self.text_system.platform_text_system.layout_line(
            " ",
            font_size,
            &[FontRun {
                len: 1,
                font_id,
                tracking: px(0.),
                word_spacing: px(0.),
            }],
        );
        let extent = (line.ascent, line.descent);
        self.asked_extents.borrow_mut().insert(key, extent);
        extent
    }

    fn clear_if_font_generation_changed(&self) -> usize {
        let font_generation = self.font_generation.load(Ordering::Acquire);
        if self.cached_font_generation.load(Ordering::Acquire) == font_generation {
            return font_generation;
        }

        let mut state = self.state.borrow_mut();
        if self.cached_font_generation.load(Ordering::Acquire) == font_generation {
            return font_generation;
        }

        *state = LineLayoutCacheState::default();
        self.asked_extents.borrow_mut().clear();
        self.cached_font_generation
            .store(font_generation, Ordering::Release);
        font_generation
    }
}

/// CSS `letter-spacing` and `word-spacing`, applied to an already-shaped line.
///
/// A post-pass rather than a shaper feature, and deliberately so: every
/// platform text system gpui has (DirectWrite, CoreText, cosmic-text) hands
/// back absolute glyph POSITIONS, so widening the gaps between them is the same
/// arithmetic on all three and needs no per-backend work at all. Painting,
/// hit-testing, alignment, `split_at` and `compute_wrap_boundaries` read those
/// same positions and `LineLayout::width`, so they all follow for free.
///
/// The unit CSS spaces is the *typographic character unit*, not the glyph: a
/// combining mark or a ligature component must not be pushed away from what it
/// belongs to. `ShapedGlyph::index` is the cluster's start offset and every
/// glyph in a cluster shares it, so a change of `index` IS a cluster boundary.
///
/// Tracking is added AFTER each unit, including the last, which is what Chrome
/// does and what makes a tracked run measure `n * tracking` wider.
fn apply_spacing_to_layout(layout: &mut LineLayout, text: &str, runs: &[FontRun]) {
    if runs
        .iter()
        .all(|run| run.tracking == px(0.) && run.word_spacing == px(0.))
    {
        return;
    }

    // `runs` are byte spans over `text`, and a shaped run is not necessarily
    // the same span (the platform may split one on a font fallback), so the
    // spacing in force is looked up by the glyph's own byte offset.
    let spacing_at = |offset: usize| {
        let mut start = 0;
        for run in runs {
            if offset < start + run.len {
                return (run.tracking, run.word_spacing);
            }
            start += run.len;
        }
        runs.last()
            .map(|run| (run.tracking, run.word_spacing))
            .unwrap_or((px(0.), px(0.)))
    };

    let mut extra = px(0.);
    let mut previous_index: Option<usize> = None;
    for run in layout.runs.iter_mut() {
        for glyph in run.glyphs.iter_mut() {
            if previous_index != Some(glyph.index) {
                if let Some(previous) = previous_index {
                    let (tracking, word_spacing) = spacing_at(previous);
                    extra += tracking;
                    // CSS adds word spacing at each word-separator character,
                    // which for every script gpui shapes is U+0020.
                    if text[previous..].starts_with(' ') {
                        extra += word_spacing;
                    }
                }
                previous_index = Some(glyph.index);
            }
            glyph.position.x += extra;
        }
    }

    if let Some(previous) = previous_index {
        let (tracking, word_spacing) = spacing_at(previous);
        extra += tracking;
        if text[previous..].starts_with(' ') {
            extra += word_spacing;
        }
    }
    layout.width += extra;
}

// Combining marks (e.g. Thai vowel signs, Arabic diacritics) are shaped by
// HarfBuzz at the same x position as their base character. The force-width
// loop must not advance the cell counter for these zero-advance glyphs,
// otherwise they get displaced into the next cell. We detect them by checking
// whether shaped x has advanced by at least half a cell beyond the last base.
fn apply_force_width_to_layout(layout: &mut LineLayout, force_width: Pixels) {
    let mut glyph_pos: usize = 0;
    // NEG_INFINITY ensures the first glyph is always classified as a base.
    let mut last_base_shaped_x = px(f32::NEG_INFINITY);
    let mut last_base_actual_x = px(0.);

    for run in layout.runs.iter_mut() {
        for glyph in run.glyphs.iter_mut() {
            let shaped_x = glyph.position.x;

            if shaped_x > last_base_shaped_x + force_width * 0.5 {
                let forced_x = glyph_pos * force_width;
                if (shaped_x - forced_x).abs() > px(1.) {
                    glyph.position.x = forced_x;
                }
                last_base_shaped_x = shaped_x;
                last_base_actual_x = glyph.position.x;
                glyph_pos += 1;
            } else {
                glyph.position.x = last_base_actual_x + (shaped_x - last_base_shaped_x);
            }
        }
    }
}

/// A run of text with a single font.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
#[expect(missing_docs)]
pub struct FontRun {
    pub len: usize,
    pub font_id: FontId,
    /// Extra space after each typographic character unit — CSS
    /// `letter-spacing`. Part of the run, and therefore part of every layout
    /// CACHE KEY, because two runs that differ only in tracking are two
    /// different layouts.
    pub tracking: Pixels,
    /// Extra space after each word separator — CSS `word-spacing`.
    pub word_spacing: Pixels,
}

/// vue-native fork: a stretch of a line shaped at one font size, or laid out as an inline
/// box. A line whose text is all at the size it is shaped at, with no inline box, has none.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub(crate) struct LayoutSpan {
    pub(crate) len: usize,
    pub(crate) font_size: Pixels,
    pub(crate) inline_box: Option<InlineBox>,
}

trait AsCacheKeyRef {
    fn as_cache_key_ref(&self) -> CacheKeyRef<'_>;
}

#[derive(Clone, Debug, Eq)]
struct CacheKey {
    text: SharedString,
    font_size: Pixels,
    runs: SmallVec<[FontRun; 1]>,
    spans: SmallVec<[LayoutSpan; 0]>,
    wrap_width: Option<Pixels>,
    max_lines: Option<usize>,
    force_width: Option<Pixels>,
    /// vue-native fork: the same text wraps differently under other rules.
    wrap: TextWrap,
}

#[derive(Copy, Clone, PartialEq, Eq, Hash)]
struct CacheKeyRef<'a> {
    text: &'a str,
    font_size: Pixels,
    runs: &'a [FontRun],
    spans: &'a [LayoutSpan],
    wrap_width: Option<Pixels>,
    max_lines: Option<usize>,
    force_width: Option<Pixels>,
    wrap: TextWrap,
}

impl PartialEq for dyn AsCacheKeyRef + '_ {
    fn eq(&self, other: &dyn AsCacheKeyRef) -> bool {
        self.as_cache_key_ref() == other.as_cache_key_ref()
    }
}

impl Eq for dyn AsCacheKeyRef + '_ {}

impl Hash for dyn AsCacheKeyRef + '_ {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.as_cache_key_ref().hash(state)
    }
}

impl AsCacheKeyRef for CacheKey {
    fn as_cache_key_ref(&self) -> CacheKeyRef<'_> {
        CacheKeyRef {
            text: &self.text,
            font_size: self.font_size,
            runs: self.runs.as_slice(),
            spans: self.spans.as_slice(),
            wrap_width: self.wrap_width,
            max_lines: self.max_lines,
            force_width: self.force_width,
            wrap: self.wrap,
        }
    }
}

impl PartialEq for CacheKey {
    fn eq(&self, other: &Self) -> bool {
        self.as_cache_key_ref().eq(&other.as_cache_key_ref())
    }
}

impl Hash for CacheKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.as_cache_key_ref().hash(state);
    }
}

impl<'a> Borrow<dyn AsCacheKeyRef + 'a> for Arc<CacheKey> {
    fn borrow(&self) -> &(dyn AsCacheKeyRef + 'a) {
        self.as_ref() as &dyn AsCacheKeyRef
    }
}

impl AsCacheKeyRef for CacheKeyRef<'_> {
    fn as_cache_key_ref(&self) -> CacheKeyRef<'_> {
        *self
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use crate::{
        Bounds, DevicePixels, Font, FontMetrics, GlyphId, NoopTextSystem, OverflowWrap,
        PlatformTextSystem, RenderGlyphParams, Result, TextRun, TextSystem, WindowTextSystem, font,
        size,
    };
    use parking_lot::Mutex;

    /// Advances half the font size per character, with 80% of the size above the baseline
    /// and 20% below; remembers the sizes glyphs are rasterized at.
    #[derive(Default)]
    pub(crate) struct ProportionalTextSystem {
        pub(crate) rasterized_sizes: Mutex<Vec<Pixels>>,
        /// Ascent, descent and units per em of the font, when not [`NoopTextSystem`]'s.
        pub(crate) metrics: Option<(f32, f32, u32)>,
        pub(crate) line_metrics: Option<(f32, f32)>,
    }

    impl PlatformTextSystem for ProportionalTextSystem {
        fn add_fonts(&self, fonts: Vec<std::borrow::Cow<'static, [u8]>>) -> Result<()> {
            NoopTextSystem.add_fonts(fonts)
        }

        fn all_font_names(&self) -> Vec<String> {
            Vec::new()
        }

        fn font_id(&self, descriptor: &Font) -> Result<FontId> {
            NoopTextSystem.font_id(descriptor)
        }

        fn font_metrics(&self, font_id: FontId) -> FontMetrics {
            let metrics = NoopTextSystem.font_metrics(font_id);
            match self.metrics {
                Some((ascent, descent, units_per_em)) => FontMetrics {
                    ascent,
                    descent,
                    units_per_em,
                    line_gap: 0.,
                    ..metrics
                },
                None => metrics,
            }
        }

        fn line_metrics(&self, _: FontId) -> Option<(f32, f32)> {
            self.line_metrics
        }

        fn typographic_bounds(&self, font_id: FontId, glyph_id: GlyphId) -> Result<Bounds<f32>> {
            NoopTextSystem.typographic_bounds(font_id, glyph_id)
        }

        fn advance(&self, font_id: FontId, glyph_id: GlyphId) -> Result<Size<f32>> {
            NoopTextSystem.advance(font_id, glyph_id)
        }

        fn glyph_for_char(&self, font_id: FontId, ch: char) -> Option<GlyphId> {
            NoopTextSystem.glyph_for_char(font_id, ch)
        }

        fn glyph_raster_bounds(&self, params: &RenderGlyphParams) -> Result<Bounds<DevicePixels>> {
            self.rasterized_sizes.lock().push(params.font_size);
            Ok(Bounds::new(
                point(DevicePixels(0), DevicePixels(0)),
                size(DevicePixels(2), DevicePixels(2)),
            ))
        }

        fn rasterize_glyph(
            &self,
            _: &RenderGlyphParams,
            bounds: Bounds<DevicePixels>,
        ) -> Result<(Size<DevicePixels>, Vec<u8>)> {
            let pixels = (bounds.size.width.0 * bounds.size.height.0) as usize;
            Ok((bounds.size, vec![255; pixels]))
        }

        fn layout_line(&self, text: &str, font_size: Pixels, _: &[FontRun]) -> LineLayout {
            let advance = font_size * 0.5;
            let glyphs: Vec<_> = text
                .char_indices()
                .enumerate()
                .map(|(n, (index, _))| ShapedGlyph {
                    id: GlyphId(1),
                    position: point(advance * n as f32, px(0.)),
                    index,
                    is_emoji: false,
                })
                .collect();
            LineLayout {
                font_size,
                width: advance * glyphs.len() as f32,
                ascent: font_size * 0.8,
                descent: font_size * 0.2,
                runs: if glyphs.is_empty() {
                    Vec::new()
                } else {
                    vec![ShapedRun::new(FontId(1), glyphs)]
                },
                len: text.len(),
            }
        }

        fn recommended_rendering_mode(&self, _: FontId, _: Pixels) -> crate::TextRenderingMode {
            crate::TextRenderingMode::Grayscale
        }
    }

    fn proportional() -> WindowTextSystem {
        WindowTextSystem::new(Arc::new(TextSystem::new(Arc::new(
            ProportionalTextSystem::default(),
        ))))
    }

    pub(crate) fn sized_runs(spans: &[(usize, Option<f32>, Option<InlineBox>)]) -> Vec<TextRun> {
        spans
            .iter()
            .map(|&(len, size, inline_box)| TextRun {
                len,
                font: font(".ZedMono"),
                font_size: size.map(px),
                inline_box,
                ..Default::default()
            })
            .collect()
    }

    fn assert_close(actual: Pixels, expected: f32, what: &str) {
        assert!(
            (actual.0 - expected).abs() < 0.001,
            "{what}: {actual:?}, expected {expected}"
        );
    }

    fn assert_box(line: LineBox, top: f32, height: f32, baseline: f32) {
        assert_close(line.top, top, "top");
        assert_close(line.height, height, "height");
        assert_close(line.baseline, baseline, "baseline");
    }

    fn glyph_xs(layout: &LineLayout) -> Vec<(usize, f32)> {
        layout
            .runs
            .iter()
            .flat_map(|run| run.glyphs.iter())
            .map(|glyph| (glyph.index, glyph.position.x.0))
            .collect()
    }

    /// The macOS system font's 1980 / -432 on a 2048-unit em.
    fn system_font(line_metrics: Option<(f32, f32)>) -> WindowTextSystem {
        WindowTextSystem::new(Arc::new(TextSystem::new(Arc::new(
            ProportionalTextSystem {
                metrics: Some((1980., -432., 2048)),
                line_metrics,
                ..Default::default()
            },
        ))))
    }

    fn snapped(size: f32, ascent_units: f32, descent_units: f32, em: f32) -> (f32, f32) {
        let (a, d) =
            snapped_line_extent(px(size * ascent_units / em), px(size * descent_units / em));
        (a.0 + d.0, a.0)
    }

    #[test]
    fn a_snapped_box_is_whole_pixels_and_never_clips_the_descender() {
        // Height and baseline of the system font's string-drawing box. The descender rule holds
        // for every size here, so 26 and 34 are a pixel taller than AppKit's 30 and 40, which
        // apply it to the system font only up to 21 pt.
        let table = [
            (9., 11., 9.),
            (10., 13., 10.),
            (11., 14., 11.),
            (12., 15., 12.),
            (13., 16., 13.),
            (15., 19., 15.),
            (16., 19., 15.),
            (17., 20., 16.),
            (20., 24., 19.),
            (26., 31., 25.),
            (34., 41., 33.),
        ];
        for (size, height, baseline) in table {
            assert_eq!(
                snapped(size, 1980., 432., 2048.),
                (height, baseline),
                "{size} px"
            );
        }

        // Other families at 13 px, from their ascent and descent there. Inter and Literata
        // descend past a whole pixel (3.136, 4.004), so they are a pixel taller than AppKit's
        // 16 and 19, which do not grow a non-system font's box.
        for (family, ascent, descent, height, baseline) in [
            ("Nunito", 13.143, 4.589, 18., 13.),
            ("Inter", 12.594, 3.136, 17., 13.),
            ("Literata", 15.301, 4.004, 20., 15.),
            ("JetBrains Mono", 13.26, 3.9, 17., 13.),
            ("Noto Sans", 13.897, 3.809, 18., 14.),
        ] {
            assert_eq!(
                snapped(1., ascent, descent, 1.),
                (height, baseline),
                "{family}"
            );
        }
    }

    #[test]
    fn leading_above_puts_the_whole_difference_over_the_box() {
        let (a, d) = (px(13.), px(3.));
        assert_eq!(line_baseline(px(24.), a, d, LineLeading::Above), px(21.));
        assert_eq!(line_baseline(px(24.), a, d, LineLeading::Split), px(17.));
        assert_eq!(line_baseline(px(14.), a, d, LineLeading::Above), px(11.));
    }

    #[test]
    fn snapped_and_above_build_the_lines_of_a_text() {
        let text_system = system_font(None);
        let font = font(".ZedMono");
        let rules = |metrics, leading| TextWrap {
            line_box: crate::LineBoxRules {
                metrics,
                leading,
                normal: None,
            },
            ..TextWrap::default()
        };
        let boxes = |text: &str, runs: &[TextRun], line_height: f32, rules: TextWrap| {
            text_system
                .shape_text_with(text.to_owned().into(), px(13.), runs, None, None, rules)
                .unwrap()[0]
                .line_boxes(px(line_height))
        };
        let runs = sized_runs(&[(5, None, None)]);

        assert_eq!(
            text_system.normal_line_height_with(&font, px(13.), LineMetrics::Snapped),
            px(16.)
        );
        let snapped = rules(LineMetrics::Snapped, LineLeading::Split);
        assert_box(boxes("Hello", &runs, 16., snapped)[0], 0., 16., 13.);
        assert_box(boxes("Hello", &runs, 24., snapped)[0], 0., 24., 17.);
        let above = rules(LineMetrics::Snapped, LineLeading::Above);
        assert_box(boxes("Hello", &runs, 24., above)[0], 0., 24., 21.);

        // The shaped line's own descent with all the leading above: 24 − 13 × 0.2.
        let auto_above = rules(LineMetrics::Auto, LineLeading::Above);
        assert_box(boxes("Hello", &runs, 24., auto_above)[0], 0., 24., 21.4);

        // Each run's box from its own size, its line height scaled by it, the line as tall as
        // they reach above and below the shared baseline: 13 px is 21 + 3 of a 24 px line,
        // 26 px is 42 + 6 of a 48 px one.
        let mixed = sized_runs(&[(2, None, None), (2, Some(26.), None)]);
        assert_box(boxes("abCD", &mixed, 24., above)[0], 0., 48., 42.);
        // Split: 13 px reaches 17 above and 7 below, 26 px (31 tall) 33.5 and 14.5.
        assert_box(boxes("abCD", &mixed, 24., snapped)[0], 0., 48., 33.5);
    }

    #[test]
    fn snapped_metrics_come_from_the_platforms_line_metrics() {
        let text_system = system_font(Some((1521., -527.)));
        let (ascent, descent) = text_system.line_extent(FontId(1), px(16.), LineMetrics::Snapped);
        assert_eq!((ascent, descent), (px(12.), px(5.)));
        let (ascent, _) = text_system.line_extent(FontId(1), px(16.), LineMetrics::Auto);
        assert_close(ascent, 16. * 1980. / 2048., "ascent");
    }

    /// Font 1, asked for: 800 / 200 per 1000, x-height 500, a glyph half the size wide. Characters
    /// from U+3000 fall back to font 2 (1100 / 300, x-height 400, a glyph the size wide), or to
    /// font 3 (700 / 200) when `small_fallback`. A line reaches as far as the fonts it uses, as
    /// DirectWrite and CoreText report it.
    struct FallbackTextSystem {
        small_fallback: bool,
    }

    impl FallbackTextSystem {
        fn metrics(font_id: FontId) -> (f32, f32, f32) {
            match font_id.0 {
                2 => (1100., 300., 400.),
                3 => (700., 200., 500.),
                _ => (800., 200., 500.),
            }
        }

        fn font_for(&self, ch: char, asked: FontId) -> FontId {
            match ch {
                _ if ch < '\u{3000}' => asked,
                _ if self.small_fallback => FontId(3),
                _ => FontId(2),
            }
        }
    }

    impl PlatformTextSystem for FallbackTextSystem {
        fn add_fonts(&self, _: Vec<std::borrow::Cow<'static, [u8]>>) -> Result<()> {
            Ok(())
        }

        fn all_font_names(&self) -> Vec<String> {
            Vec::new()
        }

        fn font_id(&self, _: &Font) -> Result<FontId> {
            Ok(FontId(1))
        }

        fn font_metrics(&self, font_id: FontId) -> FontMetrics {
            let (ascent, descent, x_height) = Self::metrics(font_id);
            FontMetrics {
                units_per_em: 1000,
                ascent,
                descent: -descent,
                line_gap: 0.,
                x_height,
                ..NoopTextSystem.font_metrics(font_id)
            }
        }

        fn typographic_bounds(&self, font_id: FontId, glyph_id: GlyphId) -> Result<Bounds<f32>> {
            NoopTextSystem.typographic_bounds(font_id, glyph_id)
        }

        fn advance(&self, font_id: FontId, glyph_id: GlyphId) -> Result<Size<f32>> {
            NoopTextSystem.advance(font_id, glyph_id)
        }

        fn glyph_for_char(&self, font_id: FontId, ch: char) -> Option<GlyphId> {
            NoopTextSystem.glyph_for_char(font_id, ch)
        }

        fn glyph_raster_bounds(&self, _: &RenderGlyphParams) -> Result<Bounds<DevicePixels>> {
            Ok(Default::default())
        }

        fn rasterize_glyph(
            &self,
            _: &RenderGlyphParams,
            bounds: Bounds<DevicePixels>,
        ) -> Result<(Size<DevicePixels>, Vec<u8>)> {
            Ok((bounds.size, Vec::new()))
        }

        fn layout_line(&self, text: &str, font_size: Pixels, font_runs: &[FontRun]) -> LineLayout {
            let mut layout = LineLayout {
                font_size,
                len: text.len(),
                ..Default::default()
            };
            for (index, ch) in text.char_indices() {
                let font_id = self.font_for(ch, asked_font(font_runs, index));
                let (ascent, descent, _) = Self::metrics(font_id);
                layout.ascent = layout.ascent.max(font_size * (ascent / 1000.));
                layout.descent = layout.descent.max(font_size * (descent / 1000.));
                let glyph = ShapedGlyph {
                    id: GlyphId(1),
                    position: point(layout.width, px(0.)),
                    index,
                    is_emoji: false,
                };
                layout.width += font_size * if font_id == FontId(2) { 1. } else { 0.5 };
                match layout.runs.last_mut() {
                    Some(run) if run.font_id == font_id => run.glyphs.push(glyph),
                    _ => layout.runs.push(ShapedRun::new(font_id, vec![glyph])),
                }
            }
            layout
        }

        fn recommended_rendering_mode(&self, _: FontId, _: Pixels) -> crate::TextRenderingMode {
            crate::TextRenderingMode::Grayscale
        }
    }

    fn with_fallback(small_fallback: bool) -> WindowTextSystem {
        WindowTextSystem::new(Arc::new(TextSystem::new(Arc::new(FallbackTextSystem {
            small_fallback,
        }))))
    }

    fn fallback_lines(
        text_system: &WindowTextSystem,
        text: &str,
        wrap_width: Option<f32>,
        rules: TextWrap,
    ) -> (SmallVec<[LineBox; 1]>, Arc<WrappedLineLayout>) {
        let runs = [TextRun {
            len: text.len(),
            font: font("Asked"),
            ..Default::default()
        }];
        let lines = text_system
            .shape_text_with(
                text.to_owned().into(),
                px(10.),
                &runs,
                wrap_width.map(px),
                None,
                rules,
            )
            .unwrap();
        let layout = lines[0].layout.clone();
        (layout.line_boxes(px(10.)), layout)
    }

    fn normal(metrics: LineMetrics) -> TextWrap {
        TextWrap {
            line_box: crate::LineBoxRules {
                metrics,
                normal: Some(px(10.)),
                ..Default::default()
            },
            ..TextWrap::default()
        }
    }

    #[test]
    fn under_line_height_normal_a_taller_fallback_font_grows_its_line() {
        let text_system = with_fallback(false);
        // The asked font's normal line is 8 + 2 at 10 px; the fallback font's box is 11 + 3.
        let (boxes, _) =
            fallback_lines(&text_system, "ab\u{6F22}", None, normal(LineMetrics::Auto));
        assert_box(boxes[0], 0., 14., 11.);

        // Wrapped, only the line that holds the fallback glyph grows.
        let (boxes, layout) = fallback_lines(
            &text_system,
            "ab \u{6F22}",
            Some(16.),
            normal(LineMetrics::Auto),
        );
        assert_eq!(layout.wrap_boundaries.len(), 1);
        assert_box(boxes[0], 0., 10., 8.);
        assert_box(boxes[1], 10., 14., 11.);

        // Snapped: the same boxes in whole pixels.
        let (boxes, _) = fallback_lines(
            &text_system,
            "ab\u{6F22}",
            None,
            normal(LineMetrics::Snapped),
        );
        assert_box(boxes[0], 0., 14., 11.);
    }

    #[test]
    fn lines_without_a_taller_fallback_font_keep_the_asked_fonts_height() {
        let text_system = with_fallback(false);
        let (boxes, layout) = fallback_lines(&text_system, "abc", None, normal(LineMetrics::Auto));
        assert!(!layout.unwrapped_layout.has_mixed_runs());
        assert_box(boxes[0], 0., 10., 8.);

        // A declared line height is the line's, whatever font draws its glyphs.
        let (boxes, layout) = fallback_lines(&text_system, "ab\u{6F22}", None, TextWrap::default());
        assert!(!layout.unwrapped_layout.has_mixed_runs());
        assert_eq!(boxes[0].height, px(10.));

        // A fallback font that fits in the asked font's box changes nothing.
        let (boxes, layout) = fallback_lines(
            &with_fallback(true),
            "ab\u{6F22}",
            None,
            normal(LineMetrics::Auto),
        );
        assert!(!layout.unwrapped_layout.has_mixed_runs());
        assert_box(boxes[0], 0., 10., 8.);
    }

    #[test]
    fn font_size_adjust_sizes_a_fallback_font_to_the_asked_fonts_x_height() {
        let text_system = with_fallback(false);
        let rules = TextWrap {
            font_size_adjust: Some(FontAspect::ExHeight),
            ..TextWrap::default()
        };
        let (boxes, layout) = fallback_lines(&text_system, "ab\u{6F22}c", None, rules);
        let runs = &layout.unwrapped_layout.runs;
        let sizes: Vec<_> = runs
            .iter()
            .map(|run| (run.font_id, run.font_size))
            .collect();
        // 500 / 400 of 10 px.
        assert_eq!(
            sizes,
            vec![
                (FontId(1), px(10.)),
                (FontId(2), px(12.5)),
                (FontId(1), px(10.))
            ]
        );
        assert_close(
            runs[2].glyphs[0].position.x,
            10. + 12.5,
            "after the fallback glyph",
        );
        // The adjusted run keeps the line's height.
        assert_box(boxes[0], 0., 10., 8.);
    }

    #[test]
    fn a_tab_advances_to_the_next_stop_and_skips_one_closer_than_half_a_space() {
        let text_system = proportional();
        let shape = |text: &str, tab_size: TabSize| {
            let runs = sized_runs(&[(text.len(), None, None)]);
            let rules = TextWrap {
                tab_size,
                ..TextWrap::default()
            };
            let lines = text_system
                .shape_text_with(text.to_owned().into(), px(10.), &runs, None, None, rules)
                .unwrap();
            let layout = &lines[0].layout.unwrapped_layout;
            (glyph_xs(layout), layout.width)
        };

        let (xs, width) = shape("a\tb", TabSize::Spaces(4.));
        assert_eq!(xs, vec![(0, 0.), (1, 5.), (2, 20.)]);
        assert_eq!(width, px(25.));

        let (xs, _) = shape("abc\td", TabSize::Length(px(8.)));
        assert_eq!(
            xs[4],
            (4, 24.),
            "16 is 1px past the tab, under half a space"
        );

        let (xs, _) = shape("\t\tx", TabSize::INITIAL);
        assert_eq!(xs, vec![(0, 0.), (1, 40.), (2, 80.)]);
    }

    #[test]
    fn runs_at_their_own_size_are_shaped_at_it_on_one_baseline() {
        let text_system = proportional();
        let runs = sized_runs(&[(2, None, None), (2, Some(32.), None), (2, None, None)]);
        let layout = text_system.layout_line("abCDef", px(16.), &runs, None);

        assert_eq!(
            glyph_xs(&layout),
            vec![(0, 0.), (1, 8.), (2, 16.), (3, 32.), (4, 48.), (5, 56.)]
        );
        assert_eq!(layout.width, px(64.));
        assert_eq!(
            layout
                .runs
                .iter()
                .map(|run| run.font_size)
                .collect::<Vec<_>>(),
            vec![px(16.), px(32.), px(16.)]
        );
        assert_close(layout.ascent, 25.6, "ascent");
        assert_close(layout.descent, 6.4, "descent");
        assert!(layout.has_mixed_runs());

        // The 32 px run's line height scales with it: 40 px, 4 px of half-leading.
        let boxes = layout.line_boxes(&[], px(20.), LineLeading::Split);
        assert_eq!(boxes.len(), 1);
        assert_box(boxes[0], 0., 40., 29.6);

        for index in 0..=layout.len {
            assert_eq!(layout.closest_index_for_x(layout.x_for_index(index)), index);
        }
    }

    #[test]
    fn a_run_at_the_lines_own_size_takes_the_single_size_path() {
        let text_system = proportional();
        let plain = text_system.layout_line("abc", px(16.), &sized_runs(&[(3, None, None)]), None);
        let same =
            text_system.layout_line("abc", px(16.), &sized_runs(&[(3, Some(16.), None)]), None);
        assert!(Arc::ptr_eq(&plain, &same));
        assert!(!plain.has_mixed_runs());
        assert_eq!(
            plain
                .line_boxes(&[], px(20.), LineLeading::Split)
                .as_slice(),
            &[LineBox {
                top: px(0.),
                height: px(20.),
                baseline: (px(20.) - plain.ascent - plain.descent) / 2. + plain.ascent,
            }]
        );

        let larger = text_system.layout_line(
            "abc",
            px(16.),
            &sized_runs(&[(1, None, None), (2, Some(24.), None)]),
            None,
        );
        assert!(!Arc::ptr_eq(&plain, &larger));
        assert_eq!(larger.width, px(8. + 24.));
    }

    #[test]
    fn wrapped_lines_are_as_tall_as_their_largest_run() {
        let text_system = proportional();
        let text = "aa BBBB cc";
        let runs = sized_runs(&[(3, None, None), (4, Some(32.), None), (3, None, None)]);
        let lines = text_system
            .shape_text(text.into(), px(16.), &runs, Some(px(70.)), None)
            .unwrap();
        let line = &lines[0];

        let breaks: Vec<_> = line
            .wrap_boundaries()
            .iter()
            .map(|boundary| line.runs()[boundary.run_ix].glyphs[boundary.glyph_ix].index)
            .collect();
        assert_eq!(breaks, vec![3, 8]);

        let boxes = line.line_boxes(px(20.));
        assert_eq!(boxes.len(), 3);
        assert_box(boxes[0], 0., 20., 14.8);
        assert_box(boxes[1], 20., 40., 29.6);
        assert_box(boxes[2], 60., 20., 14.8);
        assert_eq!(line.size(px(20.)).height, px(80.));

        assert_eq!(
            line.position_for_index(4, px(20.)),
            Some(point(px(16.), px(20.)))
        );
        assert_eq!(
            line.position_for_index(9, px(20.)),
            Some(point(px(8.), px(60.)))
        );
        // Below a uniform second line, still inside the taller one.
        assert_eq!(
            line.closest_index_for_position(point(px(16.), px(50.)), px(20.)),
            Ok(4)
        );
        for index in [1, 2, 4, 5, 6, 9] {
            let position = line.position_for_index(index, px(20.)).unwrap();
            assert_eq!(
                line.closest_index_for_position(position + point(px(0.), px(5.)), px(20.)),
                Ok(index),
                "index {index}"
            );
        }
    }

    #[test]
    fn an_inline_box_takes_its_width_and_extent_as_one_character() {
        let text_system = proportional();
        let inline_box = InlineBox {
            width: px(30.),
            ascent: px(24.),
            descent: px(6.),
        };
        let text = "a\u{FFFC}b";
        let runs = sized_runs(&[
            (1, None, None),
            (3, None, Some(inline_box)),
            (1, None, None),
        ]);
        let layout = text_system.layout_line(text, px(16.), &runs, None);

        assert_eq!(glyph_xs(&layout), vec![(0, 0.), (1, 8.), (4, 38.)]);
        assert_eq!(layout.width, px(46.));
        assert_eq!(
            layout
                .runs
                .iter()
                .map(|run| run.inline_box)
                .collect::<Vec<_>>(),
            vec![false, true, false]
        );
        assert_close(layout.ascent, 24., "ascent");
        assert_close(layout.descent, 6., "descent");
        assert_box(
            layout.line_boxes(&[], px(20.), LineLeading::Split)[0],
            0.,
            30.,
            24.,
        );

        // A caret goes before or after the box, never into it.
        assert_eq!(layout.closest_index_for_x(px(18.)), 1);
        assert_eq!(layout.closest_index_for_x(px(30.)), 4);
        assert_eq!(layout.index_for_x(px(30.)), Some(1));

        let text = "aaaa\u{FFFC}";
        let runs = sized_runs(&[(4, None, None), (3, None, Some(inline_box))]);
        let lines = text_system
            .shape_text(text.into(), px(16.), &runs, Some(px(40.)), None)
            .unwrap();
        let line = &lines[0];
        assert_eq!(
            line.wrap_boundaries(),
            &[WrapBoundary {
                run_ix: 1,
                glyph_ix: 0
            }]
        );
        let boxes = line.line_boxes(px(20.));
        assert_box(boxes[0], 0., 20., 14.8);
        assert_box(boxes[1], 20., 30., 24.);
        assert_eq!(
            line.position_for_index(4, px(20.)),
            Some(point(px(32.), px(0.)))
        );
        assert_eq!(
            line.closest_index_for_position(point(px(20.), px(45.)), px(20.)),
            Ok(7)
        );
    }

    impl LineLayout {
        /// Upstream's wrapping entry point, as these tests call it: gpui's
        /// own rules ([`TextWrap::BREAK_WORD`]), no soft hyphens shown.
        fn compute_wrap_boundaries(
            &self,
            text: &str,
            wrap_width: Pixels,
            max_lines: Option<usize>,
            scratch: &mut WrapScratch,
        ) -> SmallVec<[WrapBoundary; 1]> {
            self.wrap(
                text,
                wrap_width,
                max_lines,
                TextWrap::BREAK_WORD,
                px(0.),
                scratch,
            )
            .boundaries
        }
    }

    #[test]
    fn shape_line_shares_its_text_with_the_layout_cache() {
        let text_system =
            WindowTextSystem::new(Arc::new(TextSystem::new(Arc::new(NoopTextSystem::new()))));
        // Long enough to be stored on the heap rather than inline.
        let text = SharedString::from("let value = compute_something(argument_one, argument_two);");
        let runs = [TextRun {
            len: text.len(),
            font: font(".ZedMono"),
            ..Default::default()
        }];

        text_system.shape_line(text.clone(), px(14.), &runs, None);
        let state = text_system.line_layout_cache.state.borrow();
        let (key, _) = state.current_frame.lines.iter().next().unwrap();
        assert_eq!(key.text.as_ptr(), text.as_ptr());
    }

    #[test]
    fn layouts_held_outside_the_cache_outlive_frames_that_dont_use_them() {
        let text_system =
            WindowTextSystem::new(Arc::new(TextSystem::new(Arc::new(NoopTextSystem::new()))));
        let shape = |text: &'static str| {
            let runs = [TextRun {
                len: text.len(),
                font: font(".ZedMono"),
                ..Default::default()
            }];
            text_system
                .shape_text(text.into(), px(14.), &runs, Some(px(100.)), None)
                .unwrap()
                .remove(0)
                .layout
        };

        let held = shape("held");
        let dropped = Arc::downgrade(&shape("dropped").unwrapped_layout);
        for _ in 0..3 {
            text_system.finish_frame();
        }

        assert!(Arc::ptr_eq(&shape("held"), &held));
        assert!(dropped.upgrade().is_none());
    }

    fn glyph_at(x: f32, index: usize) -> ShapedGlyph {
        ShapedGlyph {
            id: GlyphId(0),
            position: point(px(x), px(0.)),
            index,
            is_emoji: false,
        }
    }

    fn make_layout(glyphs: Vec<ShapedGlyph>) -> LineLayout {
        LineLayout {
            font_size: px(16.),
            width: px(100.),
            ascent: px(12.),
            descent: px(4.),
            runs: vec![ShapedRun {
                font_size: px(16.0),
                ascent: px(12.0),
                descent: px(4.0),
                inline_box: false,
                fallback: None,
                font_id: FontId(0),
                glyphs,
            }],
            len: 0,
        }
    }

    fn glyph_x_positions(layout: &LineLayout) -> Vec<f32> {
        layout.runs[0]
            .glyphs
            .iter()
            .map(|g| f32::from(g.position.x))
            .collect()
    }

    #[test]
    fn test_wrap_preserves_shaped_clusters() {
        let mut layout = make_layout(vec![
            glyph_at(0., 0),
            glyph_at(5., 0),
            glyph_at(10., 3),
            glyph_at(15., 3),
        ]);
        layout.width = px(20.);
        layout.len = "你好".len();
        assert_eq!(
            layout
                .compute_wrap_boundaries("你好", px(15.), None, &mut WrapScratch::default())
                .as_slice(),
            &[WrapBoundary {
                run_ix: 0,
                glyph_ix: 2
            }],
        );

        let mut layout = make_layout(vec![glyph_at(0., 0), glyph_at(10., 0)]);
        layout.width = px(20.);
        layout.len = 1;
        assert!(
            layout
                .compute_wrap_boundaries("a", px(10.), None, &mut WrapScratch::default())
                .is_empty()
        );
    }

    #[test]
    fn test_wrap_right_to_left_text() {
        for (text, indices, expected_boundaries) in [
            ("אבג דהו", vec![11, 9, 7, 6, 4, 2, 0], vec![4]),
            ("a אבג b", vec![0, 1, 6, 4, 2, 8, 9], vec![2, 6]),
        ] {
            let mut layout = make_layout(
                indices
                    .iter()
                    .enumerate()
                    .map(|(position, index)| glyph_at(position as f32 * 10., *index))
                    .collect(),
            );
            layout.width = px(indices.len() as f32 * 10.);
            layout.len = text.len();
            assert_eq!(
                layout.compute_wrap_boundaries(text, px(40.), None, &mut WrapScratch::default()),
                expected_boundaries
                    .into_iter()
                    .map(|glyph_ix| WrapBoundary {
                        run_ix: 0,
                        glyph_ix
                    })
                    .collect::<SmallVec<[WrapBoundary; 1]>>(),
                "{text:?}",
            );
        }
    }

    #[test]
    fn test_wrap_ignores_breaks_inside_shaped_clusters() {
        let text = "x a-bcde";
        let mut layout = make_layout(
            [0, 1, 2, 5, 6, 7]
                .into_iter()
                .map(|index| glyph_at(index as f32 * 10., index))
                .collect(),
        );
        layout.width = px(80.);
        layout.len = text.len();
        assert_eq!(
            layout
                .compute_wrap_boundaries(text, px(60.), None, &mut WrapScratch::default())
                .as_slice(),
            &[WrapBoundary {
                run_ix: 0,
                glyph_ix: 2
            }],
        );
    }

    #[test]
    fn test_force_width_latin_unchanged() {
        let cell_width = px(8.);
        let mut layout = make_layout(vec![glyph_at(0., 0), glyph_at(8., 1), glyph_at(16., 2)]);

        apply_force_width_to_layout(&mut layout, cell_width);

        let positions = glyph_x_positions(&layout);
        assert_eq!(positions, vec![0., 8., 16.]);
    }

    #[test]
    fn test_force_width_combining_marks_not_advanced() {
        let cell_width = px(8.);
        // Simulates Thai "กี" — base consonant at x=0, combining vowel also at x=0
        let mut layout = make_layout(vec![
            glyph_at(0., 0), // ก (base)
            glyph_at(0., 3), // ี (combining mark, same x)
        ]);

        apply_force_width_to_layout(&mut layout, cell_width);

        let positions = glyph_x_positions(&layout);
        assert_eq!(positions, vec![0., 0.]);
    }

    #[test]
    fn test_force_width_base_after_combining_mark() {
        let cell_width = px(8.);
        let mut layout = make_layout(vec![glyph_at(0., 0), glyph_at(0., 3), glyph_at(8., 6)]);

        apply_force_width_to_layout(&mut layout, cell_width);

        let positions = glyph_x_positions(&layout);
        assert_eq!(positions, vec![0., 0., 8.]);
    }

    #[test]
    fn test_force_width_multiple_combining_marks() {
        let cell_width = px(8.);
        // Simulates "ก้" — base + vowel + tone mark (two combining marks stacked)
        let mut layout = make_layout(vec![
            glyph_at(0., 0), // ก (base)
            glyph_at(0., 3), // vowel (combining)
            glyph_at(0., 6), // tone mark (combining)
            glyph_at(8., 9), // next base
        ]);

        apply_force_width_to_layout(&mut layout, cell_width);

        let positions = glyph_x_positions(&layout);
        assert_eq!(positions, vec![0., 0., 0., 8.]);
    }

    #[test]
    fn test_force_width_corrects_drifted_base_positions() {
        let cell_width = px(8.);
        // Font metrics don't perfectly match cell grid — glyphs drift >1px from cell boundary
        let mut layout = make_layout(vec![
            glyph_at(0.5, 0),  // within 1px tolerance, kept as-is
            glyph_at(10.2, 1), // >1px off from 8.0, corrected
            glyph_at(19.8, 2), // >1px off from 16.0, corrected
        ]);

        apply_force_width_to_layout(&mut layout, cell_width);

        let positions = glyph_x_positions(&layout);
        assert_eq!(positions, vec![0.5, 8., 16.]);
    }

    #[test]
    fn test_force_width_combining_mark_after_within_tolerance_base() {
        let cell_width = px(8.);
        // Base glyph is within 1px of grid so it keeps its shaped position.
        // The combining mark must align to the base's actual position, not the grid slot.
        let mut layout = make_layout(vec![glyph_at(0.5, 0), glyph_at(0.5, 3)]);

        apply_force_width_to_layout(&mut layout, cell_width);

        let positions = glyph_x_positions(&layout);
        assert_eq!(positions, vec![0.5, 0.5]);
    }

    /// One 10 px glyph per character of `text`; a soft hyphen and a zero
    /// width space take no room, as a shaper gives them.
    fn monospaced(text: &str) -> LineLayout {
        let mut x = 0.;
        let glyphs = text
            .char_indices()
            .map(|(i, ch)| {
                let glyph = glyph_at(x, i);
                if ch != '\u{AD}' && ch != '\u{200B}' {
                    x += 10.;
                }
                glyph
            })
            .collect();
        LineLayout {
            width: px(x),
            len: text.len(),
            ..make_layout(glyphs)
        }
    }

    const HYPHEN: Pixels = px(10.);

    fn rules(overflow_wrap: OverflowWrap, word_break: WordBreak, hyphens: Hyphens) -> TextWrap {
        TextWrap {
            overflow_wrap,
            word_break,
            hyphens,
            tab_size: crate::TabSize::INITIAL,
            line_box: Default::default(),
            font_size_adjust: None,
        }
    }

    const NORMAL: TextWrap = TextWrap {
        overflow_wrap: OverflowWrap::Normal,
        word_break: WordBreak::Normal,
        hyphens: Hyphens::Manual,
        tab_size: crate::TabSize::INITIAL,
        line_box: crate::LineBoxRules {
            metrics: LineMetrics::Auto,
            leading: LineLeading::Split,
            normal: None,
        },
        font_size_adjust: None,
    };

    /// Where `text` breaks at `width`, and how wide its widest overflowing line is.
    fn wrap(text: &str, width: f32, rules: TextWrap) -> (Vec<usize>, Vec<usize>, Pixels) {
        let w = monospaced(text).wrap(
            text,
            px(width),
            None,
            rules,
            HYPHEN,
            &mut WrapScratch::default(),
        );
        (
            w.boundaries.iter().map(|b| b.glyph_ix).collect(),
            w.hyphens.to_vec(),
            w.overflow,
        )
    }

    fn min_content(text: &str, rules: TextWrap) -> Pixels {
        monospaced(text).min_content_width(text, rules, HYPHEN)
    }

    #[test]
    fn min_content_width_is_the_widest_word() {
        assert_eq!(min_content("ab cd efg", NORMAL), px(30.));
        // Spaces end a stretch and hang: they never count.
        assert_eq!(min_content("abc   de ", NORMAL), px(30.));
        // UAX #14: a break after a solidus, never before it ("a/" + "bcd").
        assert_eq!(min_content("a/bcd", NORMAL), px(30.));
        // One word: all of it.
        assert_eq!(min_content("abcdef", NORMAL), px(60.));
        assert_eq!(min_content("   ", NORMAL), px(0.));
    }

    #[test]
    fn wrapped_at_min_content_it_breaks_at_every_opportunity_and_in_no_word() {
        let text = "ab cd efg";
        let width = min_content(text, NORMAL);
        assert_eq!(wrap(text, width.0, NORMAL).0, vec![3, 6]);
    }

    #[test]
    fn a_space_at_the_edge_hangs_instead_of_starting_the_next_line() {
        // "ab" fills the 20 px line; the space after it crosses the edge. The
        // break goes before "cd", not before the space.
        assert_eq!(wrap("ab cd", 20., NORMAL).0, vec![3]);
    }

    /// CSS Text 3 §5.5. `normal`: a word wider than the line overflows it,
    /// and the line breaks at the next opportunity. `break-word` and
    /// `anywhere`: the word is broken where it overflows.
    #[test]
    fn overflow_wrap_decides_whether_a_long_word_breaks() {
        let text = "aaaaaaaa bb";
        assert_eq!(wrap(text, 30., NORMAL), (vec![9], vec![], px(80.)));
        let break_word = rules(OverflowWrap::BreakWord, WordBreak::Normal, Hyphens::Manual);
        assert_eq!(wrap(text, 30., break_word), (vec![3, 6, 9], vec![], px(0.)));
        let anywhere = rules(OverflowWrap::Anywhere, WordBreak::Normal, Hyphens::Manual);
        assert_eq!(wrap(text, 30., anywhere).0, vec![3, 6, 9]);
        // The legacy `word-break: break-word` is `overflow-wrap: anywhere`.
        let legacy = rules(OverflowWrap::Normal, WordBreak::BreakWord, Hyphens::Manual);
        assert_eq!(wrap(text, 30., legacy).0, vec![3, 6, 9]);
        // gpui's own callers keep breaking.
        assert_eq!(wrap(text, 30., TextWrap::BREAK_WORD).0, vec![3, 6, 9]);
        // A word that fits is never broken, under any of them.
        assert_eq!(wrap("ab cd", 30., anywhere).0, vec![3]);
    }

    /// `anywhere` lowers the min-content size to one character; `break-word`
    /// does not (CSS Text 3 §5.5).
    #[test]
    fn only_anywhere_counts_its_breaks_for_min_content() {
        let anywhere = rules(OverflowWrap::Anywhere, WordBreak::Normal, Hyphens::Manual);
        let break_word = rules(OverflowWrap::BreakWord, WordBreak::Normal, Hyphens::Manual);
        let legacy = rules(OverflowWrap::Normal, WordBreak::BreakWord, Hyphens::Manual);
        assert_eq!(min_content("abcdef gh", anywhere), px(10.));
        assert_eq!(min_content("abcdef gh", legacy), px(10.));
        assert_eq!(min_content("abcdef gh", break_word), px(60.));
        // Never inside a grapheme cluster: e + a combining acute is one.
        assert_eq!(min_content("ae\u{301}b", anywhere), px(20.));
    }

    /// `word-break: break-all` breaks between any two letters, so a line
    /// fills up instead of wrapping at its last space; `keep-all` keeps CJK
    /// letters together (CSS Text 3 §5.2).
    #[test]
    fn word_break_moves_the_opportunities() {
        let break_all = rules(OverflowWrap::Normal, WordBreak::BreakAll, Hyphens::Manual);
        assert_eq!(wrap("ab cdefgh", 50., break_all).0, vec![5]);
        let break_word = rules(OverflowWrap::BreakWord, WordBreak::Normal, Hyphens::Manual);
        assert_eq!(wrap("ab cdefgh", 50., break_word).0, vec![3, 8]);
        assert_eq!(min_content("abc", break_all), px(10.));
        // Not before punctuation that never starts a line.
        assert_eq!(min_content("a,b", break_all), px(20.));

        let keep_all = rules(OverflowWrap::Normal, WordBreak::KeepAll, Hyphens::Manual);
        assert_eq!(min_content("日本語", NORMAL), px(10.));
        assert_eq!(min_content("日本語", keep_all), px(30.));
        assert_eq!(min_content("日本 語", keep_all), px(20.));
    }

    /// A soft hyphen is a break opportunity under `hyphens: manual`, after
    /// it, with a hyphen shown that the line must fit; never under `none`
    /// (CSS Text 3 §6.1).
    #[test]
    fn a_soft_hyphen_breaks_with_a_hyphen() {
        let text = "ab\u{AD}cd";
        // "ab" and its hyphen fill the 30 px line.
        assert_eq!(wrap(text, 30., NORMAL), (vec![3], vec![0], px(0.)));
        assert_eq!(min_content(text, NORMAL), px(30.));
        // Without room for the hyphen, the break does not happen there.
        assert_eq!(wrap(text, 25., NORMAL).0, Vec::<usize>::new());
        let none = rules(OverflowWrap::Normal, WordBreak::Normal, Hyphens::None);
        assert_eq!(wrap(text, 30., none), (vec![], vec![], px(40.)));
        assert_eq!(min_content(text, none), px(40.));
    }

    #[test]
    fn a_zero_width_space_is_a_break_opportunity() {
        assert_eq!(wrap("aaa\u{200B}bbb", 30., NORMAL).0, vec![4]);
        assert_eq!(min_content("aaa\u{200B}bbb", NORMAL), px(30.));
    }

    #[test]
    fn test_wrap_ignores_the_sign_of_a_zero_wrap_width_in_the_cache() {
        let runs = [FontRun {
            len: 2,
            font_id: FontId(1),
            tracking: px(0.),
            word_spacing: px(0.),
        }];

        let cold_cache = LineLayoutCache::new(
            Arc::new(TextSystem::new(Arc::new(NoopTextSystem::new()))),
            Arc::default(),
        );
        let cold = cold_cache.layout_wrapped_line(
            "ab",
            px(0.),
            &runs,
            &[],
            Some(px(0.)),
            None,
            TextWrap::BREAK_WORD,
        );

        let warm_cache = LineLayoutCache::new(
            Arc::new(TextSystem::new(Arc::new(NoopTextSystem::new()))),
            Arc::default(),
        );
        warm_cache.layout_wrapped_line(
            "ab",
            px(0.),
            &runs,
            &[],
            Some(-px(0.)),
            None,
            TextWrap::BREAK_WORD,
        );
        let warm = warm_cache.layout_wrapped_line(
            "ab",
            px(0.),
            &runs,
            &[],
            Some(px(0.)),
            None,
            TextWrap::BREAK_WORD,
        );

        assert_eq!(warm.wrap_boundaries, cold.wrap_boundaries);
    }

    #[test]
    fn test_wrap_cache_distinguishes_max_lines() {
        let text = "one two three four five six";
        let runs = [FontRun {
            len: text.len(),
            font_id: FontId(1),
            tracking: px(0.),
            word_spacing: px(0.),
        }];
        let cache = LineLayoutCache::new(
            Arc::new(TextSystem::new(Arc::new(NoopTextSystem::new()))),
            Arc::default(),
        );

        let unclamped = cache.layout_wrapped_line(
            text,
            px(14.),
            &runs,
            &[],
            Some(px(50.)),
            None,
            TextWrap::BREAK_WORD,
        );
        assert!(unclamped.wrap_boundaries.len() > 1);

        let clamped = cache.layout_wrapped_line(
            text,
            px(14.),
            &runs,
            &[],
            Some(px(50.)),
            Some(1),
            TextWrap::BREAK_WORD,
        );
        assert!(clamped.wrap_boundaries.is_empty());
    }
}
