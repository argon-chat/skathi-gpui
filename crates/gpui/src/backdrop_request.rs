//! vue-native: what a group of backdrop readers asks of the snapshot a
//! renderer takes before drawing it. Every backend consumes the same request,
//! so they agree on what is copied, what lies under it, and what is blurred.

use crate::{
    BackdropBlurRect, Bounds, DevicePixels, EffectQuad, ScaledPixels, Scene, VnUnderlay, point,
    size,
};

/// What the values of a backdrop's blurred texture (t0) mean.
///
/// The frame holds premultiplied, sRGB-encoded values; in a half-float (HDR)
/// frame they may exceed 1.0. Its linear light is what an HDR output pass shows,
/// per channel with `a` the alpha: `decode(min(c, a)) + a * (decode(max(c / a, 1)) - 1)`,
/// `decode(c)` at `a == 0`, with `decode` the sign-preserving sRGB curve. 1.0 is
/// SDR white. The effect prelude converts both ways with the same function
/// (`sk_frame_to_linear`, `sk_linear_to_frame`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum BackdropEncoding {
    /// The frame's own values and format.
    #[default]
    Frame,
    /// With a blur, t0 is a half-float pyramid built from the snapshot's linear
    /// light, unclamped above 1.0. Without one, t0 is the snapshot as for
    /// [`Self::Frame`]. The raw and page-only snapshots stay in the frame's
    /// encoding: the shader decodes them exactly.
    Linear,
}

/// One group's backdrop: blur rects, or effect quads of one pipeline, that
/// share a kernel depth and do not overlap.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BackdropRequest {
    /// Dual-Kawase pyramid levels the snapshot is blurred to.
    pub kernel_levels: usize,
    /// The device pixels the group samples, or `None` for anywhere in the frame.
    pub samples: Option<Bounds<ScaledPixels>>,
    /// Whether the scene's underlay sources ([`VnUnderlay`]) are composited
    /// under the snapshot: only a snapshot of the window's own target has them.
    pub underlays: bool,
    /// What the blurred texture holds.
    pub encoding: BackdropEncoding,
}

impl BackdropRequest {
    /// An effect layer's effect, which may sample its backdrop anywhere.
    pub fn layer(quad: &EffectQuad) -> Self {
        Self {
            kernel_levels: quad.effective_kernel_levels() as usize,
            samples: None,
            underlays: false,
            encoding: quad.backdrop_encoding(),
        }
    }

    /// Whether the blurred texture is a linear half-float pyramid rather than
    /// one in the frame's encoding.
    pub fn linear_blur(&self) -> bool {
        self.encoding == BackdropEncoding::Linear && self.kernel_levels > 0
    }

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
            underlays: false,
            encoding: BackdropEncoding::Frame,
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
            underlays: false,
            encoding: quads
                .first()
                .map_or(BackdropEncoding::Frame, EffectQuad::backdrop_encoding),
        }
    }

    /// The same request, on the window's own target (`root`) or not.
    pub fn on_root(self, root: bool) -> Self {
        Self {
            underlays: root,
            ..self
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

    /// The underlays to composite under this snapshot, topmost first: the
    /// order destination-over draws them in, so the last painted ends on top.
    /// Empty unless [`Self::underlays`]; only those with a picture that meets
    /// [`Self::region`].
    pub fn underlays<'a>(&self, scene: &'a Scene) -> Vec<&'a VnUnderlay> {
        if !self.underlays {
            return Vec::new();
        }
        let region = self.region();
        scene
            .vn_underlays
            .iter()
            .rev()
            .filter(|underlay| {
                underlay.has_picture()
                    && region.is_none_or(|region| vn_meets(&underlay.visible_bounds(), &region))
            })
            .collect()
    }
}

/// Whether device-pixel `bounds` overlap the whole pixels of `region`.
pub fn vn_meets(bounds: &Bounds<ScaledPixels>, region: &Bounds<DevicePixels>) -> bool {
    let (left, top) = (region.origin.x.0 as f32, region.origin.y.0 as f32);
    let (right, bottom) = (
        left + region.size.width.0 as f32,
        top + region.size.height.0 as f32,
    );
    !bounds.is_empty()
        && bounds.origin.x.0 < right
        && bounds.origin.y.0 < bottom
        && bounds.origin.x.0 + bounds.size.width.0 > left
        && bounds.origin.y.0 + bounds.size.height.0 > top
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
    use crate::{
        AtlasTextureId, AtlasTextureKind, AtlasTile, Corners, EFFECT_FLAG_BACKDROP_REACH,
        EFFECT_FLAG_NEEDS_BACKDROP, TileId, vn_underlay_flags,
    };

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

    pub(crate) fn underlay(x: f32, key: u64, picture: bool) -> VnUnderlay {
        let bounds = Bounds::new(
            point(ScaledPixels(x), ScaledPixels(0.)),
            size(ScaledPixels(40.), ScaledPixels(40.)),
        );
        VnUnderlay {
            order: 0,
            sampling: 0,
            tile: AtlasTile {
                texture_id: AtlasTextureId {
                    index: 0,
                    kind: AtlasTextureKind::Polychrome,
                },
                tile_id: TileId(0),
                padding: 0,
                bounds: Bounds::default(),
            },
            bounds,
            clip: bounds,
            corner_radii: Corners::default(),
            content: bounds,
            opacity: 1.,
            flags: if picture {
                vn_underlay_flags::PICTURE
            } else {
                0
            },
            key: [key as u32, (key >> 32) as u32],
            pad: [0; 2],
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

    #[test]
    fn a_root_snapshot_composites_the_pictures_its_region_meets_topmost_first() {
        let mut scene = Scene::default();
        scene.vn_insert_underlay(underlay(0., 1, true));
        scene.vn_insert_underlay(underlay(20., 2, true));
        scene.vn_insert_underlay(underlay(30., 3, false));
        scene.vn_insert_underlay(underlay(400., 4, true));
        let request = BackdropRequest::effects(&[effect(10., Some(4.))]);
        assert!(request.underlays(&scene).is_empty(), "not on the root");
        let keys: Vec<u64> = request
            .on_root(true)
            .underlays(&scene)
            .iter()
            .map(|u| u.key())
            .collect();
        assert_eq!(keys, [2, 1], "no picture, or out of reach, is left out");
        let whole = BackdropRequest::effects(&[effect(10., None)]).on_root(true);
        assert_eq!(whole.underlays(&scene).len(), 3);
    }

    #[test]
    fn only_root_readers_mark_what_they_sample_as_read() {
        let mut scene = Scene::default();
        scene.vn_insert_underlay(underlay(0., 1, false));
        scene.vn_insert_underlay(underlay(600., 2, true));
        let mut quad = effect(10., Some(4.));
        quad.content_mask.bounds = Bounds::new(
            point(ScaledPixels(0.), ScaledPixels(0.)),
            size(ScaledPixels(1000.), ScaledPixels(1000.)),
        );
        scene.insert_primitive(quad);
        scene.finish();
        assert_eq!(scene.vn_underlays_read().collect::<Vec<_>>(), [1]);

        let mut plain = Scene::default();
        plain.vn_insert_underlay(underlay(0., 1, true));
        plain.finish();
        assert_eq!(plain.vn_underlays_read().count(), 0, "nothing reads");
    }

    #[test]
    fn submitted_pictures_follow_the_composited_tile_across_scene_reuse() {
        use std::{cell::Cell, rc::Rc};

        let frame = Rc::new(Cell::new(11));
        let mut first = Scene::default();
        first.vn_insert_underlay_frame(underlay(0., 1, true), Some(frame.clone()));
        first.vn_insert_underlay_frame(underlay(20., 2, true), Some(Rc::new(Cell::new(18))));
        first.finish();

        let mut replay = Scene::default();
        replay.replay(0..first.paint_operations.len(), &first);
        replay.finish();
        let picture = &replay.vn_underlays[0];
        replay.vn_begin_underlay_frame();
        replay.vn_drew_underlay(picture);
        frame.set(12);
        replay.vn_submit_underlays();
        assert_eq!(replay.vn_take_submitted_pictures().as_slice(), [(1, 11)]);

        for _ in 0..2 {
            replay.vn_begin_underlay_frame();
            replay.vn_drew_underlay(picture);
            replay.vn_drew_underlay(picture);
            replay.vn_submit_underlays();
            assert_eq!(replay.vn_take_submitted_pictures().as_slice(), [(1, 12)]);
        }
        replay.vn_begin_underlay_frame();
        replay.vn_drew_underlay(picture);
        assert!(
            replay.vn_take_submitted_pictures().is_empty(),
            "present skipped"
        );
        replay.vn_begin_underlay_frame();
        replay.vn_submit_underlays();
        assert!(replay.vn_take_submitted_pictures().is_empty(), "no reader");
    }
}
