//! The keyboard: global shortcuts (rebindable in settings) and the Alt+Tab
//! window switcher.
//!
//! Alt+Tab can't be taken with `RegisterHotKey`, so a low-level keyboard
//! hook watches for it on a thread of its own: the engine thread sometimes
//! waits on other apps, and a hook that answers late gets dropped by
//! Windows. The hook only posts messages to the engine.

use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU8, Ordering};

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::*;

use super::{HOTKEY_FIND, HOTKEY_OVERVIEW, HOTKEY_QUIT, HOTKEY_TIDY, WM_APP_SWITCH, WM_APP_SWITCH_END, WM_APP_TYPE};
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
    post_lp(msg, wp, 0);
}

fn post_lp(msg: u32, wp: usize, lp: isize) {
    let hwnd = HWND(ENGINE.load(Ordering::Acquire) as *mut _);
    unsafe {
        let _ = PostMessageW(Some(hwnd), msg, WPARAM(wp), LPARAM(lp));
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
    if code == HC_ACTION as i32 {
        let k = &*(lp.0 as *const KBDLLHOOKSTRUCT);
        if launcher(k, wp.0 as u32) {
            return LRESULT(1);
        }
        if peek(k, wp.0 as u32) {
            return LRESULT(1);
        }
        if ENABLED.load(Ordering::Acquire) && handle(k, wp.0 as u32) == Some(true) {
            return LRESULT(1);
        }
    }
    CallNextHookEx(None, code, wp, lp)
}

// ---- the launcher ----------------------------------------------------------

/// `WM_APP_TYPE` codes: a character typed (in `lparam`), an editing key
/// for the search, or a lone tap of the Windows key.
pub const TYPE_CHAR: usize = 0;
pub const TYPE_BACK: usize = 1;
pub const TYPE_ENTER: usize = 2;
pub const TYPE_ESC: usize = 3;
pub const TYPE_UP: usize = 4;
pub const TYPE_DOWN: usize = 5;
pub const TYPE_WIN: usize = 6;

/// The overview is up with the pointer on empty canvas: typing searches.
static TYPE_ARMED: AtomicBool = AtomicBool::new(false);
/// The search was opened from here and the panel doesn't have the keyboard
/// yet: keys keep coming this way until it does (or the search closes).
static CAPTURING: AtomicBool = AtomicBool::new(false);
/// The overview is up: a lone Windows key tap opens the search.
static WIN_ARMED: AtomicBool = AtomicBool::new(false);
/// The Windows key: 0 up, 1 down on its own, 2 down with another key.
static WIN: AtomicU8 = AtomicU8::new(0);
/// The overlay. Typing only searches with the pointer over it, not over an
/// app on another monitor.
static OVERLAY: AtomicIsize = AtomicIsize::new(0);

pub fn set_launcher_armed(typing: bool, win: bool, overlay: HWND) {
    TYPE_ARMED.store(typing, Ordering::Release);
    WIN_ARMED.store(win, Ordering::Release);
    OVERLAY.store(overlay.0 as isize, Ordering::Release);
}

pub fn set_capturing(on: bool) {
    CAPTURING.store(on, Ordering::Release);
}

/// A lone Windows key tap in the overview, and typing over empty canvas,
/// go to the search. Everything else (Win+anything, keys with Ctrl or Alt
/// held, a launcher's own hotkey) passes untouched. True swallows the key.
fn launcher(k: &KBDLLHOOKSTRUCT, msg: u32) -> bool {
    if k.dwExtraInfo == OWN_KEYS {
        return false;
    }
    let down = matches!(msg, WM_KEYDOWN | WM_SYSKEYDOWN);
    let vk = VIRTUAL_KEY(k.vkCode as u16);
    if matches!(vk, VK_LWIN | VK_RWIN) {
        if down {
            let _ = WIN.compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire);
            return false;
        }
        if WIN.swap(0, Ordering::AcqRel) == 1 && WIN_ARMED.load(Ordering::Acquire) {
            // Windows opens Start when the key comes up with nothing pressed
            // in between; an unassigned key in between stops that, as any
            // Win+key combination does. The release itself still goes out.
            send_keys(&[(VIRTUAL_KEY(VK_MASK), false), (VIRTUAL_KEY(VK_MASK), true), (vk, true)]);
            post(WM_APP_TYPE, TYPE_WIN);
            return true;
        }
        return false;
    }
    if !down {
        return false;
    }
    let _ = WIN.compare_exchange(1, 2, Ordering::AcqRel, Ordering::Acquire);
    let capturing = CAPTURING.load(Ordering::Acquire);
    if !capturing && !(TYPE_ARMED.load(Ordering::Acquire) && pointer_on_overlay()) {
        return false;
    }
    let held = |v: VIRTUAL_KEY| unsafe { GetAsyncKeyState(v.0 as i32) } < 0;
    if [VK_CONTROL, VK_MENU, VK_LWIN, VK_RWIN].into_iter().any(held) {
        return false;
    }
    if capturing {
        let code = match vk {
            VK_BACK => Some(TYPE_BACK),
            VK_RETURN => Some(TYPE_ENTER),
            VK_ESCAPE => Some(TYPE_ESC),
            VK_UP => Some(TYPE_UP),
            VK_DOWN => Some(TYPE_DOWN),
            VK_TAB if held(VK_SHIFT) => Some(TYPE_UP),
            VK_TAB => Some(TYPE_DOWN),
            _ => None,
        };
        if let Some(code) = code {
            post(WM_APP_TYPE, code);
            return true;
        }
    }
    let Some(c) = typed_char(k) else { return false };
    // A space on its own is no search (and may be the start of a peek).
    if c.is_whitespace() && !capturing {
        return false;
    }
    CAPTURING.store(true, Ordering::Release);
    post_lp(WM_APP_TYPE, TYPE_CHAR, c as isize);
    true
}

fn pointer_on_overlay() -> bool {
    let mut p = windows::Win32::Foundation::POINT::default();
    unsafe { GetCursorPos(&mut p).is_ok() && WindowFromPoint(p).0 as isize == OVERLAY.load(Ordering::Acquire) }
}

/// The character the key types with the current Shift and Caps Lock, in
/// the keyboard layout of the app in front. None for dead keys, keys that
/// type nothing and control characters.
fn typed_char(k: &KBDLLHOOKSTRUCT) -> Option<char> {
    unsafe {
        let thread = GetWindowThreadProcessId(GetForegroundWindow(), None);
        let layout = GetKeyboardLayout(thread);
        let mut state = [0u8; 256];
        if GetAsyncKeyState(VK_SHIFT.0 as i32) < 0 {
            state[VK_SHIFT.0 as usize] = 0x80;
        }
        if GetKeyState(VK_CAPITAL.0 as i32) & 1 != 0 {
            state[VK_CAPITAL.0 as usize] = 1;
        }
        let mut buf = [0u16; 4];
        // Flag 4: leave the keyboard's dead-key state alone, so accents
        // typed into the app in front still come out right.
        if ToUnicodeEx(k.vkCode, k.scanCode, &state, &mut buf, 4, Some(layout)) != 1 {
            return None;
        }
        char::from_u32(buf[0] as u32).filter(|c| !c.is_control())
    }
}

// ---- peek ------------------------------------------------------------------

/// `WM_APP_PEEK` codes: Space went down over a window (a hold may follow),
/// came up quickly (a tap: the space was typed after all), or was held and
/// is now let go.
pub const PEEK_PENDING: usize = 0;
pub const PEEK_TAP: usize = 1;
pub const PEEK_END: usize = 2;

/// The overview is up with the pointer on a window: Space may peek.
static PEEK_ARMED: AtomicBool = AtomicBool::new(false);
/// Space: 0 up (or not ours), 1 down and held back, 2 held long enough:
/// peeking until it comes up.
static SPACE: AtomicU8 = AtomicU8::new(0);

pub fn set_peek_armed(on: bool) {
    PEEK_ARMED.store(on, Ordering::Release);
}

/// The engine's hold timer ran out: true if Space is still down and held
/// back, which makes it a peek (a release from here on ends it).
pub fn peek_held() -> bool {
    SPACE.compare_exchange(1, 2, Ordering::AcqRel, Ordering::Acquire).is_ok()
}

/// Space held over a window in the overview peeks at it. Until it's clearly
/// a hold, the press is held back; a quick tap, or another key pressed with
/// it (typing), sends the space on after all. True swallows the event.
fn peek(k: &KBDLLHOOKSTRUCT, msg: u32) -> bool {
    if k.dwExtraInfo == OWN_KEYS {
        return false;
    }
    let down = matches!(msg, WM_KEYDOWN | WM_SYSKEYDOWN);
    let vk = VIRTUAL_KEY(k.vkCode as u16);
    let state = SPACE.load(Ordering::Acquire);
    if vk == VK_SPACE {
        if down {
            if state != 0 {
                // Auto-repeat while held back or peeking.
                return true;
            }
            let held = |v: VIRTUAL_KEY| unsafe { GetAsyncKeyState(v.0 as i32) } < 0;
            let mods = [VK_CONTROL, VK_MENU, VK_LWIN, VK_RWIN, VK_SHIFT].into_iter().any(held);
            if !PEEK_ARMED.load(Ordering::Acquire) || mods {
                return false;
            }
            SPACE.store(1, Ordering::Release);
            post(super::WM_APP_PEEK, PEEK_PENDING);
            return true;
        }
        return match SPACE.swap(0, Ordering::AcqRel) {
            1 => {
                post(super::WM_APP_PEEK, PEEK_TAP);
                send_keys(&[(VK_SPACE, false), (VK_SPACE, true)]);
                true
            }
            2 => {
                post(super::WM_APP_PEEK, PEEK_END);
                true
            }
            _ => false,
        };
    }
    // Typing: another key while Space is held back. The space came first,
    // so it goes first, then this key.
    if down && state == 1 && SPACE.compare_exchange(1, 0, Ordering::AcqRel, Ordering::Acquire).is_ok() {
        post(super::WM_APP_PEEK, PEEK_TAP);
        send_keys(&[(VK_SPACE, false), (VK_SPACE, true), (vk, false)]);
        return true;
    }
    false
}

/// Marks the keys `send_keys` types, so the hook lets them by.
const OWN_KEYS: usize = 0x4150_4B59;

/// Types keys: (key, release).
fn send_keys(keys: &[(VIRTUAL_KEY, bool)]) {
    let inputs: Vec<INPUT> = keys
        .iter()
        .map(|&(vk, up)| INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: vk,
                    dwFlags: if up { KEYEVENTF_KEYUP } else { KEYBD_EVENT_FLAGS(0) },
                    dwExtraInfo: OWN_KEYS,
                    ..Default::default()
                },
            },
        })
        .collect();
    unsafe {
        SendInput(&inputs, std::mem::size_of::<INPUT>() as i32);
    }
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
