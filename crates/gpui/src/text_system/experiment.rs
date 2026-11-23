//! vue-native fork: how glyphs are positioned and when transformed text is rasterized again.
//!
//! `SK_TEXT_EXPERIMENT`, a comma-separated list read once, turns the defaults off for bisecting
//! (`no-phases`, `no-settle`); `log` makes the platform log what it rasterizes with.

use std::sync::OnceLock;
use std::time::{Duration, Instant};

use collections::FxHashMap;

use crate::{Point, SUBPIXEL_VARIANTS_X, SUBPIXEL_VARIANTS_Y, ScaledPixels, TransformationMatrix};

const PHASE_DENOMINATOR: u8 = 60;
/// How long a transform holds still before its text is rasterized at its device size.
pub(crate) const SETTLE: Duration = Duration::from_millis(200);
/// A transform not painted for this long is in motion again when it comes back.
const GAP: Duration = Duration::from_millis(100);

#[derive(Debug)]
#[allow(missing_docs)]
pub struct TextExperiment {
    pub phases: bool,
    pub settle: bool,
    pub log: bool,
}

/// The switches in force: the defaults, changed by `SK_TEXT_EXPERIMENT`.
pub fn text_experiment() -> &'static TextExperiment {
    static EXPERIMENT: OnceLock<TextExperiment> = OnceLock::new();
    EXPERIMENT.get_or_init(|| {
        // CoreGraphics is asked for the shift itself (#104); the web build rasterizes elsewhere.
        let phases = cfg!(not(any(target_os = "macos", target_family = "wasm")));
        let mut experiment = TextExperiment {
            phases,
            settle: true,
            log: false,
        };
        let Ok(value) = std::env::var("SK_TEXT_EXPERIMENT") else {
            return experiment;
        };
        for switch in value.split(',').map(str::trim) {
            match switch {
                "no-phases" => experiment.phases = false,
                "no-settle" => experiment.settle = false,
                "log" => experiment.log = true,
                "" => {}
                other => log::warn!("SK_TEXT_EXPERIMENT: unknown switch {other:?}"),
            }
        }
        log::info!("SK_TEXT_EXPERIMENT: {experiment:?}");
        experiment
    })
}

/// Horizontal phases for a size: a step never coarser than 3 % of the em.
fn phases_for(ppem: f32) -> u8 {
    (1.0 / (0.03 * ppem)).ceil().clamp(1.0, 5.0) as u8
}

/// A glyph's whole-pixel origin and its subpixel variant, for an origin in device pixels.
pub(crate) fn quantize_glyph_origin(
    origin: Point<ScaledPixels>,
    ppem: f32,
) -> (Point<ScaledPixels>, Point<u8>) {
    if !text_experiment().phases {
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
    let step = |value: f32, q: u8| -> (f32, u8) {
        let steps = crate::util::round_half_toward_zero(value * q as f32) as i64;
        (
            steps.div_euclid(q as i64) as f32,
            steps.rem_euclid(q as i64) as u8,
        )
    };
    let q = phases_for(ppem);
    let (x, phase_x) = step(origin.x.0, q);
    let (y, phase_y) = step(origin.y.0, SUBPIXEL_VARIANTS_Y);
    (
        Point::new(ScaledPixels(x), ScaledPixels(y)),
        Point::new(phase_x * (PHASE_DENOMINATOR / q), phase_y),
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

/// How a glyph under a transform is placed.
pub(crate) enum GlyphTransform {
    /// Moved by its transform's translation, untransformed, or resampled under the transform
    /// when it scales or rotates.
    Default,
    /// Resampled under the transform while it moves.
    Pending,
    /// Rasterized `scale` times larger at the transformed origin, untransformed.
    Fold { scale: f32 },
}

struct Seen {
    first: Instant,
    last: Instant,
    settled: bool,
}

/// Per window: which transforms text has been painted under, and since when.
#[derive(Default)]
pub(crate) struct TextSettle {
    seen: FxHashMap<[u32; 6], Seen>,
    last_pending: Option<Instant>,
    timer_armed: bool,
    settle_frame: bool,
}

fn transform_bits(transform: &TransformationMatrix) -> [u32; 6] {
    let [[a, b], [c, d]] = transform.rotation_scale;
    let [x, y] = transform.translation;
    [a, b, c, d, x, y].map(f32::to_bits)
}

impl TextSettle {
    /// The placement for a glyph under `transform`. A `rigid` run (`will-change: transform`)
    /// keeps a translation as its transform while it moves, as a scaled run does.
    pub(crate) fn placement(
        &mut self,
        transform: &TransformationMatrix,
        rigid: bool,
    ) -> GlyphTransform {
        if !text_experiment().settle || transform.is_unit() {
            return GlyphTransform::Default;
        }
        let [[a, b], [c, d]] = transform.rotation_scale;
        if !(b == 0.0 && c == 0.0 && a == d && a > 0.0) || (a == 1.0 && !rigid) {
            return GlyphTransform::Default;
        }
        let now = Instant::now();
        let settle_frame = self.settle_frame;
        let seen = self.seen.entry(transform_bits(transform)).or_insert(Seen {
            first: now,
            last: now,
            settled: false,
        });
        if !seen.settled {
            // A cached subtree is not painted while it holds still, so the frame the settle
            // timer asks for sees a gap that is no motion.
            if !settle_frame && now - seen.last > GAP {
                seen.first = now;
            }
            seen.settled = now - seen.first >= SETTLE;
        }
        seen.last = now;
        if seen.settled {
            GlyphTransform::Fold { scale: a }
        } else {
            self.last_pending = Some(now);
            GlyphTransform::Pending
        }
    }

    /// Whether a timer must be started to repaint once the pending transforms settle.
    pub(crate) fn arm_timer(&mut self) -> bool {
        let arm = self.last_pending.is_some() && !self.timer_armed;
        self.timer_armed |= arm;
        arm
    }

    /// How long the timer still waits, or `None` when it has nothing left to wait for.
    pub(crate) fn remaining(&self, now: Instant) -> Option<Duration> {
        let last = self.last_pending?;
        Some(SETTLE.saturating_sub(now.saturating_duration_since(last)))
    }

    /// The timer fired with nothing painted in motion for `SETTLE`: the next frame repaints
    /// everything, cached subtrees included, and rasterizes what held still.
    pub(crate) fn begin_settle_frame(&mut self) {
        self.timer_armed = false;
        self.last_pending = None;
        self.settle_frame = true;
    }

    pub(crate) fn disarm(&mut self) {
        self.timer_armed = false;
    }

    pub(crate) fn end_frame(&mut self) {
        self.settle_frame = false;
        if self.seen.len() > 1024 {
            let now = Instant::now();
            self.seen
                .retain(|_, seen| now - seen.last < Duration::from_secs(10));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phases_follow_the_size() {
        let counts: Vec<u8> = [11.0, 12.0, 16.0, 17.0, 33.0, 34.0, 48.0]
            .iter()
            .map(|&p| phases_for(p))
            .collect();
        assert_eq!(counts, [4, 3, 3, 2, 2, 1, 1]);
    }

    #[test]
    fn a_variant_rasterizes_at_the_position_it_was_quantized_from() {
        if !text_experiment().phases {
            return;
        }
        for ppem in [11.0, 13.0, 20.0, 40.0] {
            for x in [10.0, 10.1, 10.37, 10.5, 10.74, 10.9, -3.3] {
                let (whole, variant) =
                    quantize_glyph_origin(Point::new(ScaledPixels(x), ScaledPixels(4.6)), ppem);
                let at = whole.x.0 + subpixel_variant_offset(variant).x;
                let step = 1.0 / phases_for(ppem) as f32;
                assert!(
                    (at - x).abs() <= step / 2.0 + 1e-4,
                    "{x} at {ppem} ppem lands at {at}"
                );
                assert_eq!(whole.y.0, 5.0);
            }
        }
    }

    fn scale(s: f32) -> TransformationMatrix {
        TransformationMatrix {
            rotation_scale: [[s, 0.0], [0.0, s]],
            translation: [0.0, 0.0],
        }
    }

    #[test]
    fn a_changing_transform_never_settles_and_a_still_one_does() {
        if !text_experiment().settle {
            return;
        }
        let mut settle = TextSettle::default();
        for i in 1..20 {
            let moving = scale(1.0 + i as f32 / 100.0);
            assert!(matches!(
                settle.placement(&moving, false),
                GlyphTransform::Pending
            ));
            std::thread::sleep(Duration::from_millis(15));
        }
        let still = scale(1.14);
        assert!(matches!(
            settle.placement(&still, false),
            GlyphTransform::Pending
        ));
        assert!(settle.arm_timer());
        assert!(!settle.arm_timer());
        std::thread::sleep(SETTLE + Duration::from_millis(20));
        assert_eq!(settle.remaining(Instant::now()), Some(Duration::ZERO));
        settle.begin_settle_frame();
        assert!(matches!(
            settle.placement(&still, false),
            GlyphTransform::Fold { .. }
        ));
        settle.end_frame();
        assert_eq!(settle.remaining(Instant::now()), None);
    }

    #[test]
    fn a_transform_that_comes_back_after_a_gap_starts_over() {
        if !text_experiment().settle {
            return;
        }
        let mut settle = TextSettle::default();
        let turn = scale(1.14);
        settle.placement(&turn, false);
        std::thread::sleep(SETTLE + Duration::from_millis(20));
        assert!(matches!(
            settle.placement(&turn, false),
            GlyphTransform::Pending
        ));
    }

    #[test]
    fn only_a_rigid_run_keeps_a_translation_and_settles_it() {
        if !text_experiment().settle {
            return;
        }
        let mut settle = TextSettle::default();
        let moved = TransformationMatrix {
            rotation_scale: [[1.0, 0.0], [0.0, 1.0]],
            translation: [3.3, 1.4],
        };
        assert!(matches!(
            settle.placement(&moved, false),
            GlyphTransform::Default
        ));
        assert!(matches!(
            settle.placement(&moved, true),
            GlyphTransform::Pending
        ));
        std::thread::sleep(SETTLE + Duration::from_millis(20));
        settle.begin_settle_frame();
        assert!(matches!(
            settle.placement(&moved, true),
            GlyphTransform::Fold { scale } if scale == 1.0
        ));
    }
}
