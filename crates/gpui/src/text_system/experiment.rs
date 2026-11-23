//! vue-native fork: `SK_TEXT_EXPERIMENT`, a comma-separated list of switches that try text
//! rendering practices against today's behaviour, read once. Unset, everything is as before.
//!
//! - `phases`: horizontal subpixel phases by size, `ceil(1 / (0.03 ppem))` clamped to 1..=5.
//! - `floor`: a phase is `floor((x + 0.001) q) / q` instead of the nearest of the `q` phases.
//! - `contrast=flat|mac|half`: the enhanced contrast as a function of the text colour's linear
//!   luminance (Windows shader).
//! - `rigid`: a translated run keeps its transform and is resampled, as a scaled one is.
//! - `rerast`: a uniformly scaled run is rasterized at its device size on every frame.
//! - `settle`: transformed text is resampled while its transform changes and rasterized at its
//!   device size once the transform has held still for `SETTLE`.
//! - `log`: the platform logs what it rasterizes with.

use std::cell::RefCell;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use collections::FxHashMap;

use crate::{Point, SUBPIXEL_VARIANTS_X, SUBPIXEL_VARIANTS_Y, ScaledPixels, TransformationMatrix};

const PHASE_DENOMINATOR: u8 = 60;
const SETTLE: Duration = Duration::from_millis(200);

#[derive(Default, Debug)]
#[allow(missing_docs)]
pub struct TextExperiment {
    pub phases: bool,
    pub floor: bool,
    /// 0 off, 1 `flat`, 2 `mac`, 3 `half`.
    pub contrast: u32,
    pub rigid: bool,
    pub rerast: bool,
    pub settle: bool,
    pub log: bool,
}

/// The switches `SK_TEXT_EXPERIMENT` turns on.
pub fn text_experiment() -> &'static TextExperiment {
    static EXPERIMENT: OnceLock<TextExperiment> = OnceLock::new();
    EXPERIMENT.get_or_init(|| {
        let mut experiment = TextExperiment::default();
        let Ok(value) = std::env::var("SK_TEXT_EXPERIMENT") else {
            return experiment;
        };
        for switch in value.split(',').map(str::trim) {
            match switch {
                "phases" => experiment.phases = true,
                "floor" => experiment.floor = true,
                "contrast=flat" => experiment.contrast = 1,
                "contrast" | "contrast=mac" => experiment.contrast = 2,
                "contrast=half" => experiment.contrast = 3,
                "rigid" => experiment.rigid = true,
                "rerast" => experiment.rerast = true,
                "settle" => experiment.settle = true,
                "log" => experiment.log = true,
                "" => {}
                other => log::warn!("SK_TEXT_EXPERIMENT: unknown switch {other:?}"),
            }
        }
        log::info!("SK_TEXT_EXPERIMENT: {experiment:?}");
        experiment
    })
}

fn phases_for(ppem: f32) -> u8 {
    (1.0 / (0.03 * ppem)).ceil().clamp(1.0, 5.0) as u8
}

/// Under `log`, how many glyph origins lie within 0.0005 px of where rounding to the nearest of
/// four phases flips, or where `floor((x + 0.001) q)` does, and how often the two rules disagree.
fn record_phase_stats(x: f32) {
    use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
    static COUNTS: [AtomicU64; 5] = [const { AtomicU64::new(0) }; 5];
    let q = SUBPIXEL_VARIANTS_X as f32;
    let scaled = x * q;
    let near_round = (scaled - scaled.floor() - 0.5).abs() < 0.0005 * q;
    let near_floor = ((x + 0.001) * q - ((x + 0.001) * q).round()).abs() < 0.0005 * q;
    let on_grid = (scaled - scaled.round()).abs() < 0.0001 * q;
    let round = crate::util::round_half_toward_zero(scaled);
    let floor = ((x + 0.001) * q).floor();
    let counts = [true, near_round, near_floor, round != floor, on_grid];
    for (count, hit) in COUNTS.iter().zip(counts) {
        if hit {
            count.fetch_add(1, Relaxed);
        }
    }
    let total = COUNTS[0].load(Relaxed);
    if total % 2000 == 0 {
        log::info!(
            "SK_TEXT_EXPERIMENT: phase stats after {total} glyphs: near a rounding flip {}, near a floor flip {}, round != floor {}, on a quarter pixel {}",
            COUNTS[1].load(Relaxed),
            COUNTS[2].load(Relaxed),
            COUNTS[3].load(Relaxed),
            COUNTS[4].load(Relaxed)
        );
    }
}

/// A glyph's whole-pixel origin and its subpixel variant, for an origin in device pixels.
pub(crate) fn quantize_glyph_origin(
    origin: Point<ScaledPixels>,
    ppem: f32,
) -> (Point<ScaledPixels>, Point<u8>) {
    let experiment = text_experiment();
    if experiment.log {
        record_phase_stats(origin.x.0);
    }
    if !experiment.phases && !experiment.floor {
        let quantized = Point::new(
            crate::util::round_half_toward_zero(origin.x.0 * SUBPIXEL_VARIANTS_X as f32)
                / SUBPIXEL_VARIANTS_X as f32,
            crate::util::round_half_toward_zero(origin.y.0 * SUBPIXEL_VARIANTS_Y as f32)
                / SUBPIXEL_VARIANTS_Y as f32,
        );
        let variant = Point::new(
            (quantized.x.fract() * SUBPIXEL_VARIANTS_X as f32) as u8,
            (quantized.y.fract() * SUBPIXEL_VARIANTS_Y as f32) as u8,
        );
        return (quantized.map(|c| ScaledPixels(c.trunc())), variant);
    }
    let q = if experiment.phases {
        phases_for(ppem)
    } else {
        SUBPIXEL_VARIANTS_X
    };
    let step = |value: f32, q: u8| -> (f32, u8) {
        let steps = if experiment.floor {
            ((value + 0.001) * q as f32).floor()
        } else {
            crate::util::round_half_toward_zero(value * q as f32)
        } as i64;
        (
            steps.div_euclid(q as i64) as f32,
            steps.rem_euclid(q as i64) as u8,
        )
    };
    let (x, phase_x) = step(origin.x.0, q);
    let (y, phase_y) = step(origin.y.0, SUBPIXEL_VARIANTS_Y);
    let variant_x = if experiment.phases {
        phase_x * (PHASE_DENOMINATOR / q)
    } else {
        phase_x
    };
    (
        Point::new(ScaledPixels(x), ScaledPixels(y)),
        Point::new(variant_x, phase_y),
    )
}

/// The offset in device pixels a subpixel variant stands for.
pub fn subpixel_variant_offset(variant: Point<u8>) -> Point<f32> {
    let x_denominator = if text_experiment().phases {
        PHASE_DENOMINATOR
    } else {
        SUBPIXEL_VARIANTS_X
    };
    Point::new(
        variant.x as f32 / x_denominator as f32,
        variant.y as f32 / SUBPIXEL_VARIANTS_Y as f32,
    )
}

thread_local! {
    static SEEN: RefCell<FxHashMap<[u32; 6], (Instant, Instant)>> = RefCell::default();
}

fn transform_bits(transform: &TransformationMatrix) -> [u32; 6] {
    let [[a, b], [c, d]] = transform.rotation_scale;
    let [x, y] = transform.translation;
    [a, b, c, d, x, y].map(f32::to_bits)
}

/// Has `transform` been painted with continuously for `SETTLE`? A transform last painted more
/// than a few frames ago starts over.
fn settled(transform: &TransformationMatrix) -> bool {
    let now = Instant::now();
    SEEN.with(|seen| {
        let mut seen = seen.borrow_mut();
        if seen.len() > 4096 {
            seen.retain(|_, (_, last)| now - *last < SETTLE);
        }
        let entry = seen.entry(transform_bits(transform)).or_insert((now, now));
        if now - entry.1 > Duration::from_millis(100) {
            entry.0 = now;
        }
        entry.1 = now;
        now - entry.0 >= SETTLE
    })
}

/// How a glyph under `transform` is placed.
pub(crate) enum GlyphTransform {
    /// Today's placement.
    Default,
    /// Draw at `origin` (local device pixels) under `transform`, resampled.
    Resample,
    /// Rasterize `scale` times larger at the transformed origin, untransformed.
    Fold { scale: f32 },
    /// Resample for now and paint again until the transform settles.
    Pending,
}

/// The placement the switches choose for a glyph under `transform`.
pub(crate) fn glyph_transform(transform: &TransformationMatrix) -> GlyphTransform {
    let experiment = text_experiment();
    if !(experiment.rigid || experiment.rerast || experiment.settle) || transform.is_unit() {
        return GlyphTransform::Default;
    }
    let [[a, b], [c, d]] = transform.rotation_scale;
    let uniform = b == 0.0 && c == 0.0 && a == d && a > 0.0;
    let translate_only = uniform && a == 1.0;
    if experiment.settle {
        if !uniform {
            return GlyphTransform::Default;
        }
        return if settled(transform) {
            GlyphTransform::Fold { scale: a }
        } else {
            GlyphTransform::Pending
        };
    }
    if experiment.rerast && uniform && !translate_only {
        return GlyphTransform::Fold { scale: a };
    }
    if experiment.rigid && translate_only {
        return GlyphTransform::Resample;
    }
    GlyphTransform::Default
}
