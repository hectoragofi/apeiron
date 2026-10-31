//! The keyboard: global shortcuts (rebindable in settings) and the Alt+Tab
//! window switcher.
//!
//! Alt+Tab can't be taken with `RegisterHotKey`, so a low-level keyboard
//! hook watches for it on a thread of its own: the engine thread sometimes
//! waits on other apps, and a hook that answers late gets dropped by
//! Windows. The hook only posts messages to the engine.

use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::*;

use super::{HOTKEY_FIND, HOTKEY_OVERVIEW, HOTKEY_QUIT, HOTKEY_TIDY, WM_APP_SWITCH, WM_APP_SWITCH_END};
use crate::settings::Settings;

/// Switcher messages' `wparam`.
pub const STEP_NEXT: usize = 0;
pub const STEP_BACK: usize = 1;
pub const END_ACCEPT: usize = 1;
pub const END_CANCEL: usize = 0;

/// Alt+Tab is ours (setting on, not paused).
static ENABLED: AtomicBool = AtomicBool::new(false);
/// Alt is held and Tab was pressed: the switcher is running.
static SWITCHING: AtomicBool = AtomicBool::new(false);
static ENGINE: AtomicIsize = AtomicIsize::new(0);

/// An unassigned virtual key, tapped when the switcher starts so the Alt
/// release that ends it doesn't open the app's menu bar.
const VK_MASK: u16 = 0xE8;

pub fn set_switcher_enabled(on: bool) {
    ENABLED.store(on, Ordering::Release);
    if !on {
        SWITCHING.store(false, Ordering::Release);
    }
}

/// The shortcuts as set, with the names of any Windows refused (another app
/// has them). Unregisters the old ones first; with `suspended`, registers
/// nothing (the settings window is recording a new one).
pub fn register_hotkeys(hwnd: HWND, s: &Settings, suspended: bool) -> Vec<String> {
    let bindings = [
        (HOTKEY_OVERVIEW, &s.key_overview),
        (HOTKEY_FIND, &s.key_find),
        (HOTKEY_TIDY, &s.key_tidy),
        (HOTKEY_QUIT, &s.key_quit),
    ];
    let mut failed = Vec::new();
    for (id, key) in bindings {
        unsafe {
            let _ = UnregisterHotKey(Some(hwnd), id);
        }
        if suspended {
            continue;
        }
        let Some((mods, vk)) = crate::hotkey::parse(key) else { continue };
        let mods = HOT_KEY_MODIFIERS(mods) | MOD_NOREPEAT;
        if let Err(e) = unsafe { RegisterHotKey(Some(hwnd), id, mods, vk) } {
            log!("warning: couldn't register {key}: {}", e.message());
            failed.push(key.clone());
        }
    }
    failed
}

/// Starts the hook thread; switcher messages go to `engine`.
pub fn spawn_hook(engine: HWND) {
    ENGINE.store(engine.0 as isize, Ordering::Release);
    let started = std::thread::Builder::new().name("keys".into()).spawn(|| unsafe {
        let hook = match SetWindowsHookExW(WH_KEYBOARD_LL, Some(on_key), None, 0) {
            Ok(h) => h,
            Err(e) => {
                log!("warning: no Alt+Tab switcher: {}", e.message());
                return;
            }
        };
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        let _ = UnhookWindowsHookEx(hook);
    });
    if let Err(e) = started {
        log!("warning: no keyboard thread: {e}");
    }
}

fn post(msg: u32, wp: usize) {
    let hwnd = HWND(ENGINE.load(Ordering::Acquire) as *mut _);
    unsafe {
        let _ = PostMessageW(Some(hwnd), msg, WPARAM(wp), LPARAM(0));
    }
}

fn tap_mask_key() {
    let key = |flags| INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 { ki: KEYBDINPUT { wVk: VIRTUAL_KEY(VK_MASK), dwFlags: flags, ..Default::default() } },
    };
    unsafe {
        SendInput(&[key(KEYBD_EVENT_FLAGS(0)), key(KEYEVENTF_KEYUP)], std::mem::size_of::<INPUT>() as i32);
    }
}

unsafe extern "system" fn on_key(code: i32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 && ENABLED.load(Ordering::Acquire) {
        let k = &*(lp.0 as *const KBDLLHOOKSTRUCT);
        if let Some(swallow) = handle(k, wp.0 as u32) {
            if swallow {
                return LRESULT(1);
            }
        }
    }
    CallNextHookEx(None, code, wp, lp)
}

/// Some(true) swallows the key, Some(false)/None lets it through.
fn handle(k: &KBDLLHOOKSTRUCT, msg: u32) -> Option<bool> {
    if k.flags.contains(LLKHF_INJECTED) {
        return None;
    }
    let down = matches!(msg, WM_KEYDOWN | WM_SYSKEYDOWN);
    let vk = VIRTUAL_KEY(k.vkCode as u16);
    let switching = SWITCHING.load(Ordering::Acquire);
    if vk == VK_TAB && down && k.flags.contains(LLKHF_ALTDOWN) {
        let shift = unsafe { GetAsyncKeyState(VK_SHIFT.0 as i32) } < 0;
        if !switching {
            SWITCHING.store(true, Ordering::Release);
            tap_mask_key();
        }
        post(WM_APP_SWITCH, if shift { STEP_BACK } else { STEP_NEXT });
        return Some(true);
    }
    if !switching {
        return None;
    }
    match vk {
        VK_MENU | VK_LMENU | VK_RMENU if !down => {
            SWITCHING.store(false, Ordering::Release);
            post(WM_APP_SWITCH_END, END_ACCEPT);
            Some(false)
        }
        VK_ESCAPE if down => {
            SWITCHING.store(false, Ordering::Release);
            post(WM_APP_SWITCH_END, END_CANCEL);
            Some(true)
        }
        VK_LEFT | VK_UP if down => {
            post(WM_APP_SWITCH, STEP_BACK);
            Some(true)
        }
        VK_RIGHT | VK_DOWN if down => {
            post(WM_APP_SWITCH, STEP_NEXT);
            Some(true)
        }
        VK_SHIFT | VK_LSHIFT | VK_RSHIFT | VK_MENU | VK_LMENU | VK_RMENU => None,
        // Anything else while switching would reach the app as Alt+key.
        _ => Some(true),
    }
}
