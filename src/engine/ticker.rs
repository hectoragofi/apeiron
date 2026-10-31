//! Animation frames paced by the compositor.
//!
//! `SetTimer` can't go below the system timer resolution (~15.6 ms), so
//! glides would run at ~64 Hz with uneven steps. Instead a helper thread
//! waits for each DWM present (`DwmFlush`) and posts `WM_APP_TICK` to the
//! engine window, only while something moves; idle, it's parked.
//!
//! Input moves the camera and windows between frames but leaves the
//! thumbnails alone: [`Pacer`] notes that the view is stale and the next
//! tick applies it once, so a 125 Hz touchpad or a 1000 Hz mouse moves
//! things exactly once per display refresh.
//!
//! While running it raises the system timer resolution to 1 ms: otherwise
//! the fallback sleep (and gpui's) rounds up to the ~15.6 ms default tick.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::Thread;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::Graphics::Dwm::DwmFlush;
use windows::Win32::Media::{timeBeginPeriod, timeEndPeriod};
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
        unsafe {
            timeBeginPeriod(1);
        }
        match &self.thread {
            Some(t) => t.unpark(),
            None => unsafe {
                SetTimer(Some(self.hwnd), TIMER_TICK, 8, None);
            },
        }
    }

    pub fn stop(&self) {
        if !self.state.running.swap(false, Ordering::AcqRel) {
            return;
        }
        unsafe {
            timeEndPeriod(1);
        }
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

/// Idle frames the ticker keeps running for before it stops, so input that
/// comes in bursts doesn't start and stop it every frame.
const IDLE_FRAMES: u32 = 8;
/// Fingers or a drag that hold the camera keep the ticker going this long
/// after their last motion.
const HOLD: Duration = Duration::from_millis(500);

/// What the per-frame tick still owes the screen.
#[derive(Default)]
pub struct Pacer {
    /// The camera or windows moved since the view was last updated.
    view: bool,
    idle: u32,
    last_input: Option<Instant>,
}

impl Pacer {
    /// Input moved things: update the view on the next frame.
    pub fn queue(&mut self, now: Instant) {
        self.view = true;
        self.idle = 0;
        self.last_input = Some(now);
    }

    pub fn pending(&self) -> bool {
        self.view
    }

    /// The view was just brought up to date.
    pub fn applied(&mut self) {
        self.view = false;
    }

    /// After a frame: whether the ticker is still needed. `busy`: an
    /// animation or resize wants every frame; `holding`: fingers or a drag
    /// own the camera, so more motion is likely.
    pub fn keep_going(&mut self, busy: bool, holding: bool, now: Instant) -> bool {
        let held = holding && self.last_input.is_some_and(|t| now.saturating_duration_since(t) < HOLD);
        if busy || held || self.view {
            self.idle = 0;
            return true;
        }
        self.idle += 1;
        if self.idle < IDLE_FRAMES {
            return true;
        }
        self.idle = 0;
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frames_until_stop(p: &mut Pacer, busy: bool, holding: bool, now: Instant) -> u32 {
        (1..=100).find(|_| !p.keep_going(busy, holding, now)).unwrap_or(u32::MAX)
    }

    #[test]
    fn queued_view_is_applied_once() {
        let mut p = Pacer::default();
        let now = Instant::now();
        p.queue(now);
        p.queue(now);
        assert!(p.pending());
        p.applied();
        assert!(!p.pending());
    }

    #[test]
    fn a_pending_view_keeps_the_ticker() {
        let mut p = Pacer::default();
        let now = Instant::now();
        p.queue(now);
        for _ in 0..50 {
            assert!(p.keep_going(false, false, now));
        }
        p.applied();
        assert_eq!(frames_until_stop(&mut p, false, false, now), IDLE_FRAMES);
    }

    #[test]
    fn busy_never_stops_and_idle_stops_after_a_few_frames() {
        let mut p = Pacer::default();
        let now = Instant::now();
        assert_eq!(frames_until_stop(&mut p, true, false, now), u32::MAX);
        assert_eq!(frames_until_stop(&mut p, false, false, now), IDLE_FRAMES);
        // Counting starts afresh once stopped.
        assert_eq!(frames_until_stop(&mut p, false, false, now), IDLE_FRAMES);
    }

    #[test]
    fn holding_lasts_only_while_input_is_recent() {
        let mut p = Pacer::default();
        let t0 = Instant::now();
        p.queue(t0);
        p.applied();
        assert_eq!(frames_until_stop(&mut p, false, true, t0 + HOLD / 2), u32::MAX);
        assert_eq!(frames_until_stop(&mut p, false, true, t0 + HOLD * 2), IDLE_FRAMES);
        // Holding without any input yet doesn't keep it.
        let mut fresh = Pacer::default();
        assert_eq!(frames_until_stop(&mut fresh, false, true, t0), IDLE_FRAMES);
    }
}
