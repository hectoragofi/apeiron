//! Small, quick animations: things fade and settle into place as they
//! appear. Each plays once per element id, so an id that changes (a tab
//! becoming the shown one, a new menu) plays it again.

use std::time::Duration;

use gpui::*;

pub const FAST: Duration = Duration::from_millis(140);
pub const NORMAL: Duration = Duration::from_millis(240);

fn ease() -> impl Fn(f32) -> f32 {
    ease_out_quint()
}

/// Fades in, up to `to` opacity.
pub fn fade<E: IntoElement + Styled + 'static>(
    el: E,
    id: impl Into<ElementId>,
    d: Duration,
    to: f32,
) -> AnimationElement<E> {
    el.with_animation(id, Animation::new(d).with_easing(ease()), move |el, t| el.opacity(t * to))
}

/// Fades in while sliding `dy` pixels to its `top` (an absolute element).
pub fn rise<E: IntoElement + Styled + 'static>(
    el: E,
    id: impl Into<ElementId>,
    d: Duration,
    top: Pixels,
    dy: f32,
    to: f32,
) -> AnimationElement<E> {
    el.with_animation(id, Animation::new(d).with_easing(ease()), move |el, t| {
        el.top(top + px(dy * (1. - t))).opacity(t * to)
    })
}

/// A whole-number key for element ids from a window id and a few flags.
pub fn key(id: isize, salt: u64) -> u64 {
    (id as u64).wrapping_mul(31).wrapping_add(salt)
}
