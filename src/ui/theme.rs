//! Colours and sizes: dark, calm, one accent.

// The palette is shared with the feature modules; not all of it is used yet.
#![allow(dead_code)]

use gpui::{rgb, rgba, Hsla};

pub const BG: u32 = 0x121316;
pub const DOT: u32 = 0x383a40;
pub const ACCENT: u32 = 0x6c8cff;
pub const TEXT: u32 = 0xe6e8ee;
pub const TEXT_DIM: u32 = 0x8b909c;
/// RGBA.
pub const PANEL: u32 = 0x1c1d22e6;
pub const BORDER: u32 = 0xffffff18;

/// Dot grid spacing at zoom 1, in physical pixels.
pub const GRID_STEP: f64 = 48.0;
/// Dots never get closer than this on screen; spacing doubles instead.
pub const GRID_MIN_STEP: f64 = 24.0;
/// Dot size in physical pixels.
pub const DOT_SIZE: f64 = 2.0;

pub fn color(c: u32) -> Hsla {
    rgb(c).into()
}

pub fn color_a(c: u32) -> Hsla {
    rgba(c).into()
}
