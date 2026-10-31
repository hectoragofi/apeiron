//! Frame timing for tuning, on with `APEIRON_TRACE=1`: once a second while
//! the ticker runs it logs how many ticks there were, how long they took
//! (95th percentile and worst) and how many frames the UI drew. Off, a tick
//! pays for one check of a cached flag.

use std::sync::OnceLock;
use std::time::{Duration, Instant};

use crate::shared::Shared;

pub fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var("APEIRON_TRACE").is_ok_and(|v| v == "1"))
}

#[derive(Default)]
pub struct FrameTrace {
    /// Start of the current second; None while the ticker is stopped.
    since: Option<Instant>,
    /// Tick durations this second, ms.
    ticks: Vec<f64>,
}

impl FrameTrace {
    pub fn begin(&self) -> Option<Instant> {
        enabled().then(Instant::now)
    }

    pub fn end(&mut self, start: Option<Instant>, shared: &Shared) {
        let Some(start) = start else { return };
        let now = Instant::now();
        if self.since.is_none() {
            // Frames drawn while idle don't count.
            shared.take_drawn();
            self.since = Some(start);
        }
        self.ticks.push((now - start).as_secs_f64() * 1000.0);
        if self.since.is_some_and(|s| now - s >= Duration::from_secs(1)) {
            let (p95, max) = summary(&mut self.ticks);
            log!("trace: {} ticks, p95 {p95:.2} ms, max {max:.2} ms, {} drawn", self.ticks.len(), shared.take_drawn());
            self.ticks.clear();
            self.since = Some(now);
        }
    }

    /// The ticker stopped: the next run starts a fresh second.
    pub fn stopped(&mut self) {
        if self.since.take().is_some() {
            self.ticks.clear();
        }
    }
}

/// 95th percentile and maximum; sorts `ms`.
fn summary(ms: &mut [f64]) -> (f64, f64) {
    if ms.is_empty() {
        return (0.0, 0.0);
    }
    ms.sort_by(f64::total_cmp);
    let k = ((ms.len() as f64 * 0.95).ceil() as usize).clamp(1, ms.len()) - 1;
    (ms[k], ms[ms.len() - 1])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summary_picks_the_95th_percentile_and_max() {
        let mut ms: Vec<f64> = (1..=100).rev().map(f64::from).collect();
        assert_eq!(summary(&mut ms), (95.0, 100.0));
        assert_eq!(summary(&mut [3.0]), (3.0, 3.0));
        assert_eq!(summary(&mut []), (0.0, 0.0));
        assert_eq!(summary(&mut [1.0, 9.0]), (9.0, 9.0));
    }
}
