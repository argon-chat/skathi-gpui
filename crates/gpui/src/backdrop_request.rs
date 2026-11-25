//! vue-native: what a group of backdrop readers asks of the snapshot a
//! renderer takes before drawing it. Every backend consumes the same request,
//! so they agree on what is copied and blurred.

use crate::{BackdropBlurRect, Bounds, DevicePixels, EffectQuad, ScaledPixels, point, size};

/// One group's backdrop: blur rects, or effect quads of one pipeline, that
/// share a kernel depth and do not overlap.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BackdropRequest {
    /// Dual-Kawase pyramid levels the snapshot is blurred to.
    pub kernel_levels: usize,
    /// The device pixels the group samples, or `None` for anywhere in the frame.
    pub samples: Option<Bounds<ScaledPixels>>,
}

impl BackdropRequest {
    /// Blur rects composite only their own bounds.
    pub fn blur_rects(rects: &[BackdropBlurRect]) -> Self {
        Self {
            kernel_levels: rects
                .first()
                .map_or(0, |rect| rect.effective_kernel_levels() as usize),
            samples: rects
                .iter()
                .map(|rect| rect.bounds)
                .reduce(|union, bounds| union.union(&bounds)),
        }
    }

    /// Effects sample within their declared reach, or anywhere when one of them
    /// declares none.
    pub fn effects(quads: &[EffectQuad]) -> Self {
        Self {
            kernel_levels: quads
                .first()
                .map_or(0, |quad| quad.effective_kernel_levels() as usize),
            samples: quads
                .iter()
                .map(EffectQuad::backdrop_samples)
                .reduce(|union, samples| Some(union?.union(&samples?)))
                .flatten(),
        }
    }

    /// The frame pixels the snapshot has to hold, not yet cut to the frame:
    /// `samples` rounded out and grown by the blur's reach. `None` is the
    /// whole frame.
    pub fn region(&self) -> Option<Bounds<DevicePixels>> {
        let samples = self.samples?;
        let reach = backdrop_blur_reach(self.kernel_levels);
        let clamp = |value: f32| value.clamp(i32::MIN as f32 / 2., i32::MAX as f32 / 2.) as i32;
        let left = clamp(samples.origin.x.0.floor()).saturating_sub(reach);
        let top = clamp(samples.origin.y.0.floor()).saturating_sub(reach);
        let right = clamp((samples.origin.x.0 + samples.size.width.0).ceil()).saturating_add(reach);
        let bottom =
            clamp((samples.origin.y.0 + samples.size.height.0).ceil()).saturating_add(reach);
        Some(Bounds {
            origin: point(DevicePixels(left), DevicePixels(top)),
            size: size(
                DevicePixels(right.saturating_sub(left)),
                DevicePixels(bottom.saturating_sub(top)),
            ),
        })
    }
}

/// Frame pixels a blur of `levels` pyramid levels can carry into a pixel. Each
/// pass reads a texel to either side of its own, `2 * 2^level` frame pixels at
/// level `level`, both chains together stay below `4 * 2^levels`, and the
/// composite's bilinear read adds a few more.
pub fn backdrop_blur_reach(levels: usize) -> i32 {
    (4 << levels) + 8
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{EFFECT_FLAG_BACKDROP_REACH, EFFECT_FLAG_NEEDS_BACKDROP};

    fn effect(x: f32, reach: Option<f32>) -> EffectQuad {
        EffectQuad {
            bounds: Bounds::new(
                point(ScaledPixels(x), ScaledPixels(10.)),
                size(ScaledPixels(20.), ScaledPixels(20.)),
            ),
            flags: EFFECT_FLAG_NEEDS_BACKDROP | reach.map_or(0, |_| EFFECT_FLAG_BACKDROP_REACH),
            backdrop_reach: ScaledPixels(reach.unwrap_or(0.)),
            ..Default::default()
        }
    }

    #[test]
    fn bounded_effects_ask_for_their_grown_union_and_one_unbounded_for_the_frame() {
        let bounded = BackdropRequest::effects(&[effect(10., Some(4.)), effect(50., Some(2.5))]);
        let reach = backdrop_blur_reach(0);
        assert_eq!(
            bounded.region(),
            Some(Bounds {
                origin: point(DevicePixels(6 - reach), DevicePixels(6 - reach)),
                size: size(
                    DevicePixels(73 + 2 * reach - 6),
                    DevicePixels(34 + 2 * reach - 6)
                ),
            })
        );
        let mixed = BackdropRequest::effects(&[effect(10., Some(4.)), effect(50., None)]);
        assert_eq!(mixed.region(), None);
    }
}
