use gpui::{Pixels, Point};

pub(super) struct PinchMotion {
    pub(super) position: Point<Pixels>,
    scale: f32,
}

impl PinchMotion {
    pub(super) fn new(position: Point<Pixels>) -> Self {
        Self {
            position,
            scale: 1.0,
        }
    }

    // Both protocols send relative centre motion and a scale relative to the start.
    pub(super) fn update(&mut self, movement: Point<Pixels>, scale: f32) -> Option<f32> {
        self.position += movement;
        if !scale.is_finite() || scale <= 0.0 {
            return None;
        }
        let delta = scale / self.scale - 1.0;
        self.scale = scale;
        Some(delta)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{point, px};

    #[test]
    fn tracks_the_centre_between_scale_changes() {
        let mut pinch = PinchMotion::new(point(px(20.0), px(10.0)));
        assert_eq!(pinch.update(point(px(4.0), px(-3.0)), 2.0), Some(1.0));
        assert_eq!(pinch.update(point(px(3.0), px(1.0)), 2.0), Some(0.0));
        assert_eq!(pinch.update(point(px(-2.0), px(0.5)), 1.0), Some(-0.5));
        assert_eq!(pinch.position, point(px(25.0), px(8.5)));
    }
}
