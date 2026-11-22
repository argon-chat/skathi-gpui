//! vue-native fork: how many drawables a macOS window's `CAMetalLayer` may hold.

use std::sync::atomic::{AtomicU8, Ordering};

static COUNT: AtomicU8 = AtomicU8::new(3);

/// macOS: the `maximumDrawableCount` of window layers configured after this call, 2 or 3 (Core
/// Animation's range; other values are clamped). The default is 3.
pub fn set_metal_drawable_count(count: u8) {
    COUNT.store(count.clamp(2, 3), Ordering::Release);
}

/// What [`set_metal_drawable_count`] set.
pub fn metal_drawable_count() -> u8 {
    COUNT.load(Ordering::Acquire)
}
