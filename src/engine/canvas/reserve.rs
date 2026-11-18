//! Earlier builds held the top of the screen for a maximized tab group's
//! bar as an app bar (like the taskbar). Maximized tab groups are left
//! alone now (the bar tucks away instead), so all that's left is giving
//! back any strip a killed run left held: Windows keeps the space otherwise.

use std::cell::Cell;

use windows::Win32::Foundation::HWND;
use windows::Win32::UI::Shell::{SHAppBarMessage, ABM_REMOVE, APPBARDATA};

thread_local! {
    static DONE: Cell<bool> = const { Cell::new(false) };
}

/// Gives back strips recorded by an earlier run, once per run.
pub(super) fn give_back_old_strips() {
    if DONE.replace(true) {
        return;
    }
    let path = crate::settings::data_dir().join("strips.txt");
    let Ok(text) = std::fs::read_to_string(&path) else { return };
    for id in text.lines().filter_map(|l| l.trim().parse::<isize>().ok()) {
        let mut abd = APPBARDATA { cbSize: std::mem::size_of::<APPBARDATA>() as u32, hWnd: HWND(id as *mut _), ..Default::default() };
        unsafe {
            SHAppBarMessage(ABM_REMOVE, &mut abd);
        }
    }
    let _ = std::fs::remove_file(path);
    log!("tabs: gave back the screen space an earlier run held");
}
