//! Window icons for labels and search results.

use std::sync::Arc;

// FEATURE C: fetch the HICON (WM_GETICON, falling back to GCLP_HICON), convert it
// to a 32x32 BGRA image and cache it per HWND (and per exe as a fallback).
#[allow(dead_code)]
pub fn icon_for(_hwnd: isize) -> Option<Arc<gpui::RenderImage>> {
    None
}
