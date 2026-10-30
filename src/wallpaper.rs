//! The desktop wallpaper as one GPUI image, so the canvas can be painted as
//! a single continuous scene.
//!
//! A worker decodes and scales the picture once (and again only when the
//! wallpaper changes) into a 2x2 mirror tile: the picture, flipped left to
//! right beside it, and both flipped upside down below. Copies of that tile
//! laid edge to edge meet seamlessly in every direction, even where the GPU
//! samples across a tile's edge (it wraps to the opposite one, which
//! mirrors it). It's one image: separate quarters show their seams.

use std::path::PathBuf;
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

use gpui::RenderImage;
use windows::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, SystemParametersInfoW, SM_CXSCREEN, SM_CYSCREEN, SPI_GETDESKWALLPAPER,
    SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS,
};

/// GPU textures stop at 16384 px; the mirror tile is two pictures across.
const MAX_PICTURE: f64 = 8192.0;

pub struct Wallpaper {
    /// The 2x2 mirror tile, BGRA, unflipped picture top-left.
    pub image: Arc<RenderImage>,
    /// One picture's size on screen at zoom 1, filling the screen the way
    /// Windows' "Fill" does, in physical pixels.
    pub w: f64,
    pub h: f64,
}

struct Loader {
    current: Mutex<Option<Arc<Wallpaper>>>,
    worker: Sender<()>,
}

static LOADER: OnceLock<Loader> = OnceLock::new();

/// Starts the worker and loads the current wallpaper; `loaded` is called
/// (on the worker) after every load, so the UI can redraw.
pub fn start(loaded: impl Fn() + Send + 'static) {
    let (tx, rx) = mpsc::channel::<()>();
    let spawned = std::thread::Builder::new().name("wallpaper".into()).spawn(move || {
        while rx.recv().is_ok() {
            // Changes come in bursts; one load covers them all.
            while rx.try_recv().is_ok() {}
            let t = Instant::now();
            let next = load().map(Arc::new);
            match &next {
                Some(wp) => log!("wallpaper loaded in {} ms ({:.0}x{:.0})", t.elapsed().as_millis(), wp.w, wp.h),
                None => log!("no wallpaper to show; the canvas uses the dot grid"),
            }
            if let Some(loader) = LOADER.get() {
                *loader.current.lock().unwrap_or_else(|e| e.into_inner()) = next;
            }
            loaded();
        }
    });
    if let Err(e) = spawned {
        log!("warning: no wallpaper worker, the canvas uses the dot grid: {e}");
        return;
    }
    if LOADER.set(Loader { current: Mutex::new(None), worker: tx }).is_ok() {
        reload();
    }
}

/// Loads the wallpaper again, e.g. after the user picked a new one.
pub fn reload() {
    if let Some(loader) = LOADER.get() {
        let _ = loader.worker.send(());
    }
}

/// The loaded wallpaper, if there is one.
pub fn current() -> Option<Arc<Wallpaper>> {
    LOADER.get()?.current.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

fn path() -> Option<PathBuf> {
    let mut buf = [0u16; 1024];
    unsafe {
        SystemParametersInfoW(
            SPI_GETDESKWALLPAPER,
            buf.len() as u32,
            Some(buf.as_mut_ptr().cast()),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
        .ok()?;
    }
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    // Empty for a solid colour.
    (len > 0).then(|| PathBuf::from(String::from_utf16_lossy(&buf[..len])))
}

fn load() -> Option<Wallpaper> {
    let path = path()?;
    // Windows often keeps a copy without an extension (TranscodedWallpaper),
    // so go by the content.
    let picture = image::ImageReader::open(&path)
        .and_then(|r| r.with_guessed_format())
        .map_err(|e| log!("wallpaper: can't open {}: {e}", path.display()))
        .ok()?
        .decode()
        .map_err(|e| log!("wallpaper: can't decode {}: {e}", path.display()))
        .ok()?;
    let (sw, sh) = unsafe { (GetSystemMetrics(SM_CXSCREEN) as f64, GetSystemMetrics(SM_CYSCREEN) as f64) };
    let (iw, ih) = (picture.width() as f64, picture.height() as f64);
    if iw < 1.0 || ih < 1.0 || sw < 1.0 || sh < 1.0 {
        return None;
    }
    let fill = (sw / iw).max(sh / ih);
    // Stored at most at the size it fills the screen with (zoom never goes
    // past 1); a smaller picture is left for the GPU to stretch.
    let k = fill.min(1.0).min(MAX_PICTURE / iw).min(MAX_PICTURE / ih);
    let mut rgba = picture.into_rgba8();
    if k < 1.0 {
        let (pw, ph) = (((iw * k).round() as u32).max(1), ((ih * k).round() as u32).max(1));
        rgba = image::imageops::resize(&rgba, pw, ph, image::imageops::FilterType::Triangle);
    }
    let (pw, ph) = rgba.dimensions();
    // GPUI uploads BGRA.
    for p in rgba.as_chunks_mut::<4>().0 {
        p.swap(0, 2);
        p[3] = 255;
    }
    let (w, h) = (pw as usize, ph as usize);
    let mut tile = vec![0u8; w * h * 16];
    for (k, (fx, fy)) in [(false, false), (true, false), (false, true), (true, true)].into_iter().enumerate() {
        let quarter = flip(rgba.as_raw(), w, h, fx, fy);
        let (ox, oy) = ((k % 2) * w, (k / 2) * h);
        for row in 0..h {
            let at = ((oy + row) * w * 2 + ox) * 4;
            tile[at..at + w * 4].copy_from_slice(&quarter[row * w * 4..][..w * 4]);
        }
    }
    let image = Arc::new(RenderImage::new(vec![image::Frame::new(image::RgbaImage::from_raw(pw * 2, ph * 2, tile)?)]));
    // Shown at exactly its stored size when that's what fills the screen,
    // so zoom 1 is pixel for pixel.
    let (w, h) = if k == fill { (pw as f64, ph as f64) } else { (iw * fill, ih * fill) };
    Some(Wallpaper { image, w, h })
}

/// A `w` x `h` 4-byte-per-pixel picture, mirrored left to right and/or
/// upside down.
fn flip(px: &[u8], w: usize, h: usize, x: bool, y: bool) -> Vec<u8> {
    let mut out = Vec::with_capacity(px.len());
    for row in 0..h {
        let src = &px[(if y { h - 1 - row } else { row }) * w * 4..][..w * 4];
        if x {
            for p in src.as_chunks::<4>().0.iter().rev() {
                out.extend_from_slice(p);
            }
        } else {
            out.extend_from_slice(src);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::flip;

    #[test]
    fn flip_mirrors_each_way() {
        // 2x2 picture: a b / c d, one byte per channel.
        let px: Vec<u8> = [1u8, 2, 3, 4].iter().flat_map(|&v| [v; 4]).collect();
        let first = |v: Vec<u8>| v.chunks(4).map(|p| p[0]).collect::<Vec<_>>();
        assert_eq!(first(flip(&px, 2, 2, false, false)), [1, 2, 3, 4]);
        assert_eq!(first(flip(&px, 2, 2, true, false)), [2, 1, 4, 3]);
        assert_eq!(first(flip(&px, 2, 2, false, true)), [3, 4, 1, 2]);
        assert_eq!(first(flip(&px, 2, 2, true, true)), [4, 3, 2, 1]);
    }
}
