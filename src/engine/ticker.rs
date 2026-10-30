//! Animation frames paced by the compositor.
//!
//! `SetTimer` can't go below the system timer resolution (~15.6 ms), so
//! glides would run at ~64 Hz with uneven steps. Instead a helper thread
//! waits for each DWM present (`DwmFlush`) and posts `WM_APP_TICK` to the
//! engine window, only while an animation runs; idle, it's parked.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::Thread;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::Graphics::Dwm::DwmFlush;
use windows::Win32::UI::WindowsAndMessaging::{KillTimer, PostMessageW, SetTimer};

use super::WM_APP_TICK;

/// Fallback `WM_TIMER` id when the helper thread couldn't start.
pub const TIMER_TICK: usize = 100;

#[derive(Default)]
struct State {
    running: AtomicBool,
    /// A tick is posted and not yet handled; never queue a second one.
    queued: AtomicBool,
}

#[derive(Clone)]
pub struct Ticker {
    state: Arc<State>,
    /// None when the thread couldn't be spawned; ticks come from a timer then.
    thread: Option<Thread>,
    hwnd: HWND,
}

impl Ticker {
    pub fn spawn(hwnd: HWND) -> Ticker {
        let state = Arc::new(State::default());
        let target = hwnd.0 as isize;
        let s = state.clone();
        let thread = std::thread::Builder::new()
            .name("ticker".into())
            .spawn(move || run(&s, target))
            .map(|h| h.thread().clone())
            .map_err(|e| log!("warning: no vsync ticker, animating on a timer: {e}"))
            .ok();
        Ticker { state, thread, hwnd }
    }

    pub fn start(&self) {
        if self.state.running.swap(true, Ordering::AcqRel) {
            return;
        }
        match &self.thread {
            Some(t) => t.unpark(),
            None => unsafe {
                SetTimer(Some(self.hwnd), TIMER_TICK, 8, None);
            },
        }
    }

    pub fn stop(&self) {
        self.state.running.store(false, Ordering::Release);
        if self.thread.is_none() {
            unsafe {
                let _ = KillTimer(Some(self.hwnd), TIMER_TICK);
            }
        }
    }

    /// The engine took the posted tick off its queue.
    pub fn taken(&self) {
        self.state.queued.store(false, Ordering::Release);
    }
}

fn run(state: &State, hwnd: isize) {
    loop {
        while !state.running.load(Ordering::Acquire) {
            std::thread::park();
        }
        let t = Instant::now();
        // Fails without composition (never on Windows 8+); don't spin then.
        if unsafe { DwmFlush() }.is_err() || t.elapsed() < Duration::from_millis(2) {
            std::thread::sleep(Duration::from_millis(4));
        }
        if state.running.load(Ordering::Acquire) && !state.queued.swap(true, Ordering::AcqRel) {
            let posted = unsafe { PostMessageW(Some(HWND(hwnd as *mut _)), WM_APP_TICK, WPARAM(0), LPARAM(0)) };
            if posted.is_err() {
                state.queued.store(false, Ordering::Release);
            }
        }
    }
}
