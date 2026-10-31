//! Deciding what fingers on the touchpad mean: a pan or a pinch, from how
//! the hand moved once it settled, not from the first few noisy frames.
//!
//! Pure: frames in, camera motion out, so the tests can play gestures.

use std::time::{Duration, Instant};

use crate::engine::touchpad::Contact;

use super::{
    FLICK_WINDOW, LIFT_HOLD, MAX_FLICK, PINCH_MIN, REST, SETTLE, SETTLE_FRAMES, START_SLOP, STILL_PX, WHEEL_IDLE_MS,
};

/// What a finger gesture does. Decided once the fingers have clearly started
/// moving and kept until they lift: a drag never zooms and a pinch never
/// pans. Only a finger joining (a fresh start) decides again.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(super) enum GestureKind {
    #[default]
    Undecided,
    Pan,
    Zoom,
}

/// How one touchpad frame moves the camera, in finger units.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum Motion {
    /// The fingers' centroid moved by (dx, dy).
    Pan(f64, f64),
    /// The fingers' spread grew by this factor.
    Zoom(f64),
}

/// How the fingers have moved since the motion being judged started.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct Evidence {
    /// Travel every finger shares along the centroid's direction. A drag
    /// moves all of them; a pinch, however lopsided, leaves at least one
    /// still or going the other way, so it shares (almost) none.
    shared: f64,
    /// Centroid travel.
    travel: f64,
    /// Spread change, px.
    spread: f64,
    /// ln(spread / starting spread).
    log_ratio: f64,
}

#[derive(Default)]
pub(super) struct Gesture {
    /// Fingers in the last frame.
    pub(super) prev: Vec<Contact>,
    /// When the current set of fingers came down, and frames seen since.
    set_since: Option<Instant>,
    set_frames: u32,
    /// Fingers where the motion being judged started. Moves along while the
    /// set is settling and whenever the fingers rest.
    base: Vec<Contact>,
    /// Fingers when they last moved more than STILL_PX, and when.
    still: Vec<Contact>,
    still_since: Option<Instant>,
    pub(super) kind: GestureKind,
    /// The fingers own the camera: some kind was decided. Stays set while a
    /// joining finger has the gesture deciding again.
    pub(super) engaged: bool,
    /// After a finger lifts, frames until then don't move the camera.
    hold_until: Option<Instant>,
    /// Screen point a pinch zooms about, fixed when it was recognised.
    pub(super) anchor: (f64, f64),
    last: Option<Instant>,
    /// Recent pan positions (when, accumulated finger px), for the flick
    /// velocity: averaged over a window so one jumpy frame (two reports
    /// arriving together, a finger peeling off) can't throw the camera.
    trail: Vec<(Instant, f64, f64)>,
}

impl Gesture {
    /// Takes one frame of the fingers driving the canvas and returns how the
    /// camera should move for it.
    pub(super) fn feed(&mut self, now: Instant, fingers: &[Contact]) -> Option<Motion> {
        let same = self.prev.len() == fingers.len() && self.prev.iter().zip(fingers).all(|(a, b)| a.id == b.id);
        if !same {
            // A finger joining or leaving shifts the centroid and the spread:
            // measure afresh instead of jumping.
            if self.kind != GestureKind::Undecided {
                if fingers.len() > self.prev.len() {
                    // Fingers land one at a time, and the first ones may
                    // have looked like something else: decide again.
                    self.kind = GestureKind::Undecided;
                } else {
                    // The rest of the hand relaxes as a finger lifts; that
                    // isn't motion the user meant.
                    self.hold_until = Some(now + LIFT_HOLD);
                }
            }
            self.set_since = Some(now);
            self.set_frames = 0;
            self.prev = fingers.to_vec();
            self.rebase(now);
            return None;
        }
        self.set_frames += 1;
        let prev = std::mem::replace(&mut self.prev, fingers.to_vec());
        if self.kind == GestureKind::Undecided {
            return self.decide(now);
        }
        if self.hold_until.is_some_and(|t| now < t) {
            return None;
        }
        self.hold_until = None;
        let (c0, c1) = (centroid(&prev), centroid(fingers));
        match self.kind {
            GestureKind::Undecided => None,
            GestureKind::Pan => {
                let (dx, dy) = (c1.0 - c0.0, c1.1 - c0.1);
                let (ax, ay) = self.trail.last().map_or((0.0, 0.0), |&(_, x, y)| (x, y));
                self.trail.push((now, ax + dx, ay + dy));
                // One sample older than the window stays as its start.
                let old = self.trail.iter().filter(|&&(t, ..)| now - t > FLICK_WINDOW).count();
                self.trail.drain(..old.saturating_sub(1));
                self.last = Some(now);
                Some(Motion::Pan(dx, dy))
            }
            GestureKind::Zoom => {
                self.last = Some(now);
                let (s0, s1) = (spread(&prev, c0), spread(fingers, c1));
                (s0 > 1.0 && s1 > 1.0).then_some(Motion::Zoom(s1 / s0))
            }
        }
    }

    /// Judges the motion since `base` and, once it's clear, returns all of it
    /// at once, so nothing the fingers did before the decision is lost.
    fn decide(&mut self, now: Instant) -> Option<Motion> {
        let settling = self.set_frames < SETTLE_FRAMES || self.set_since.is_none_or(|t| now - t < SETTLE);
        let moved = self.prev.iter().zip(&self.still).any(|(a, b)| (a.x - b.x).hypot(a.y - b.y) > STILL_PX);
        let rested = !moved && self.still_since.is_some_and(|t| now - t >= REST);
        if settling || rested {
            // Landing sweeps, drift and sensor noise up to here say nothing
            // about what the user is about to do.
            self.rebase(now);
            return None;
        }
        if moved {
            self.still = self.prev.clone();
            self.still_since = Some(now);
        }
        let ev = evidence(&self.base, &self.prev);
        self.kind = classify(&ev);
        let motion = match self.kind {
            GestureKind::Undecided => return None,
            GestureKind::Pan => {
                let (b, c) = (centroid(&self.base), centroid(&self.prev));
                Motion::Pan(c.0 - b.0, c.1 - b.1)
            }
            GestureKind::Zoom => Motion::Zoom(ev.log_ratio.exp()),
        };
        self.engaged = true;
        self.last = Some(now);
        self.trail.clear();
        Some(motion)
    }

    fn rebase(&mut self, now: Instant) {
        self.base = self.prev.clone();
        self.still = self.prev.clone();
        self.still_since = Some(now);
    }

    /// Flick velocity when the fingers lift: only a drag that is still
    /// moving throws the camera; letting go of a pinch never pans.
    pub(super) fn release_velocity(&self, now: Instant) -> (f64, f64) {
        let fresh = self.last.is_some_and(|t| (now - t).as_secs_f64() <= 0.08);
        let (Some(a), Some(b)) = (self.trail.first(), self.trail.last()) else { return (0.0, 0.0) };
        let span = (b.0 - a.0).as_secs_f64();
        if self.kind != GestureKind::Pan || !fresh || span < 0.015 {
            return (0.0, 0.0);
        }
        let (vx, vy) = ((b.1 - a.1) / span, (b.2 - a.2) / span);
        let k = (MAX_FLICK / vx.hypot(vy).max(1e-9)).min(1.0);
        (vx * k, vy * k)
    }
}

pub(super) fn centroid(fingers: &[Contact]) -> (f64, f64) {
    let n = fingers.len().max(1) as f64;
    (fingers.iter().map(|c| c.x).sum::<f64>() / n, fingers.iter().map(|c| c.y).sum::<f64>() / n)
}

/// Mean distance of the fingers from their centroid `c`.
fn spread(fingers: &[Contact], c: (f64, f64)) -> f64 {
    fingers.iter().map(|f| (f.x - c.0).hypot(f.y - c.1)).sum::<f64>() / fingers.len().max(1) as f64
}

/// Compares two frames of the same fingers, in the same order.
fn evidence(base: &[Contact], now: &[Contact]) -> Evidence {
    let (b, c) = (centroid(base), centroid(now));
    let (mx, my) = (c.0 - b.0, c.1 - b.1);
    let travel = mx.hypot(my);
    let shared = if travel > 1e-9 {
        base.iter()
            .zip(now)
            .map(|(p, q)| ((q.x - p.x) * mx + (q.y - p.y) * my) / travel)
            .fold(f64::INFINITY, f64::min)
            .max(0.0)
    } else {
        0.0
    };
    let (s0, s1) = (spread(base, b), spread(now, c));
    let log_ratio = if s0 > 1.0 && s1 > 1.0 { (s1 / s0).ln() } else { 0.0 };
    Evidence { shared, travel, spread: s1 - s0, log_ratio }
}

/// Tells a drag from a pinch. A pinch is a clear spread change (in px and as
/// a ratio) that dwarfs any shared travel; a drag is shared travel that
/// outweighs the spread change. Anything in between waits, and leans to a
/// drag: hands splay a little as they start moving, but a pinch's fingers
/// never all travel together.
fn classify(ev: &Evidence) -> GestureKind {
    let spread = ev.spread.abs();
    if spread > START_SLOP && ev.log_ratio.abs() > PINCH_MIN && spread > 2.0 * ev.shared {
        GestureKind::Zoom
    } else if ev.shared > START_SLOP && ev.shared >= spread {
        GestureKind::Pan
    } else if ev.travel > 3.0 * START_SLOP && ev.travel > 2.0 * spread {
        // The fingers went somewhere together, one lagging: still a drag.
        GestureKind::Pan
    } else {
        GestureKind::Undecided
    }
}

/// Whether a wheel event of this kind (pinch or scroll) belongs to the
/// current burst `(pinch, last event)`: a burst keeps the kind it started
/// with until it has been quiet for WHEEL_IDLE_MS.
pub(super) fn wheel_accepts(burst: Option<(bool, Instant)>, pinch: bool, now: Instant) -> bool {
    match burst {
        Some((kind, t)) if kind != pinch => {
            now.duration_since(t) >= Duration::from_millis(WHEEL_IDLE_MS as u64)
        }
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Frames come every 8 ms, like a 125 Hz touchpad.
    const FRAME: Duration = Duration::from_millis(8);

    /// A gesture fed one frame at a time, 8 ms apart.
    struct Pad {
        g: Gesture,
        t0: Instant,
        frame: u32,
    }

    impl Pad {
        fn new() -> Pad {
            Pad { g: Gesture::default(), t0: Instant::now(), frame: 0 }
        }

        fn now(&self) -> Instant {
            self.t0 + FRAME * self.frame
        }

        fn feed(&mut self, fingers: &[Contact]) -> Option<Motion> {
            let m = self.g.feed(self.now(), fingers);
            self.frame += 1;
            m
        }

        fn feed_all(&mut self, frames: impl IntoIterator<Item = Vec<Contact>>) -> Vec<Option<Motion>> {
            frames.into_iter().map(|f| self.feed(&f)).collect()
        }
    }

    /// The kind a fresh gesture is in after these frames.
    fn decide(frames: impl IntoIterator<Item = Vec<Contact>>) -> GestureKind {
        let mut pad = Pad::new();
        pad.feed_all(frames);
        pad.g.kind
    }

    /// Sensor noise: a deterministic wobble in [-0.5, 0.5] px.
    fn jitter(i: usize, j: usize) -> f64 {
        ((i * 7919 + j * 104_729) % 11) as f64 / 10.0 - 0.5
    }

    fn hand(points: &[(f64, f64)], i: usize) -> Vec<Contact> {
        points
            .iter()
            .enumerate()
            .map(|(j, &(x, y))| Contact { id: j as u32, x: x + jitter(i, 2 * j), y: y + jitter(i, 2 * j + 1) })
            .collect()
    }

    /// Three fingers around `c`, spread scaled by `scale`, on frame `i`.
    fn three(c: (f64, f64), scale: f64, i: usize) -> Vec<Contact> {
        let pts: Vec<_> = [(-40.0, 10.0), (0.0, -15.0), (40.0, 5.0)]
            .iter()
            .map(|&(x, y)| (c.0 + x * scale, c.1 + y * scale))
            .collect();
        hand(&pts, i)
    }

    /// Two fingers resting, a thumb `k` px closer to them than it started.
    fn thumb_pinch(k: f64, i: usize) -> Vec<Contact> {
        hand(&[(300.0, 300.0), (340.0, 300.0), (320.0, 360.0 - k)], i)
    }

    fn still(n: usize) -> impl Iterator<Item = Vec<Contact>> {
        (0..n).map(|i| three((300.0, 300.0), 1.0, i))
    }

    #[test]
    fn drag_with_spread_drift_pans() {
        // 3 px a frame sideways while the fingers creep apart.
        let frames = (0..20).map(|i| three((300.0 + 3.0 * i as f64, 300.0), 1.0 + 0.008 * i as f64, i));
        assert_eq!(decide(frames), GestureKind::Pan);
        let diagonal = (0..20).map(|i| three((300.0 + 2.0 * i as f64, 300.0 - 2.0 * i as f64), 1.0, i));
        assert_eq!(decide(diagonal), GestureKind::Pan);
    }

    #[test]
    fn symmetric_pinch_zooms() {
        let out = (0..20).map(|i| three((300.0, 300.0), 1.0 + 0.03 * i as f64, i));
        assert_eq!(decide(out), GestureKind::Zoom);
        let inward = (0..20).map(|i| three((300.0, 300.0), 1.0 - 0.03 * i as f64, i));
        assert_eq!(decide(inward), GestureKind::Zoom);
    }

    #[test]
    fn thumb_only_pinch_zooms() {
        // The centroid moves a third as far as the thumb, about as much as
        // the spread changes; nothing else moves, so it's no drag.
        let frames = (0..20).map(|i| thumb_pinch(2.0 * i as f64, i));
        assert_eq!(decide(frames), GestureKind::Zoom);
        let apart = (0..20).map(|i| thumb_pinch(-2.0 * i as f64, i));
        assert_eq!(decide(apart), GestureKind::Zoom);
    }

    #[test]
    fn lopsided_two_finger_pinch_zooms() {
        for (a, b) in [(1.6, 0.4), (2.0, 0.0), (-1.6, -0.4)] {
            // One finger does 80% (or all) of the work.
            let frames = (0..20).map(|i| {
                let t = i as f64;
                hand(&[(300.0 + a * t, 300.0), (420.0 - b * t, 300.0)], i)
            });
            assert_eq!(decide(frames), GestureKind::Zoom, "{a}/{b}");
        }
    }

    #[test]
    fn resting_noise_never_decides_and_a_later_pinch_zooms() {
        let mut pad = Pad::new();
        pad.feed_all(still(60));
        assert_eq!(pad.g.kind, GestureKind::Undecided);
        assert!(!pad.g.engaged);
        // Same fingers, now a slow lopsided pinch.
        pad.feed_all((0..40).map(|i| {
            let mut f = three((300.0, 300.0), 1.0, 60 + i);
            f[0].x += 0.8 * i as f64;
            f
        }));
        assert_eq!(pad.g.kind, GestureKind::Zoom);
    }

    #[test]
    fn a_pause_forgets_earlier_creep() {
        // The hand creeps 5 px, rests, then moves 3 px more: 8 px in all,
        // but only the 3 px since the rest count.
        let mut pad = Pad::new();
        pad.feed_all(still(5));
        pad.feed_all((1..=10).map(|i| three((300.0 + 0.5 * i as f64, 300.0), 1.0, 4 + i)));
        pad.feed_all((15..40).map(|i| three((305.0, 300.0), 1.0, i)));
        pad.feed_all((1..=4).map(|i| three((305.0 + 0.75 * i as f64, 300.0), 1.0, 39 + i)));
        assert_eq!(pad.g.kind, GestureKind::Undecided);
        // It's still a drag once it goes on.
        pad.feed_all((1..=10).map(|i| three((308.0 + 3.0 * i as f64, 300.0), 1.0, 43 + i)));
        assert_eq!(pad.g.kind, GestureKind::Pan);
    }

    #[test]
    fn front_loaded_splay_drag_pans() {
        // A wide hand (spread ~96 px) opens 7 px over its first frames while
        // barely moving, then drags without splaying further.
        let mut pad = Pad::new();
        let mut kinds = Vec::new();
        let frames = (0..6)
            .map(|i| three((300.0, 300.0), 3.0, i))
            .chain((1..=3).map(|i| three((300.0 + 0.7 * i as f64, 300.0), 3.0 + 0.07 * i as f64, 5 + i)))
            .chain((1..20).map(|i| three((302.1 + 3.0 * i as f64, 300.0), 3.21, 8 + i)));
        for f in frames {
            pad.feed(&f);
            kinds.push(pad.g.kind);
        }
        assert!(!kinds.contains(&GestureKind::Zoom));
        assert_eq!(pad.g.kind, GestureKind::Pan);
    }

    #[test]
    fn fingers_landing_one_by_one_then_dragging_pans() {
        // The second finger sweeps in while the first rests, then the third
        // lands and all three drag.
        let mut pad = Pad::new();
        pad.feed_all((0..4).map(|i| hand(&[(300.0, 300.0), (340.0 + 10.0 * i as f64, 300.0)], i)));
        assert_eq!(pad.g.kind, GestureKind::Undecided);
        pad.feed_all((0..20).map(|i| three((300.0 + 3.0 * i as f64, 300.0), 1.0, 4 + i)));
        assert_eq!(pad.g.kind, GestureKind::Pan);
    }

    #[test]
    fn a_finger_joining_decides_again() {
        // Two fingers pinch long enough to zoom, then a third lands and the
        // hand drags: that's a pan now, and the fingers still own the camera.
        let mut pad = Pad::new();
        pad.feed_all((0..20).map(|i| hand(&[(300.0, 300.0), (340.0 + 2.0 * i as f64, 300.0)], i)));
        assert_eq!(pad.g.kind, GestureKind::Zoom);
        let m = pad.feed(&three((300.0, 300.0), 1.0, 20));
        assert_eq!(m, None);
        assert_eq!(pad.g.kind, GestureKind::Undecided);
        assert!(pad.g.engaged);
        pad.feed_all((1..20).map(|i| three((300.0 + 3.0 * i as f64, 300.0), 1.0, 20 + i)));
        assert_eq!(pad.g.kind, GestureKind::Pan);
    }

    #[test]
    fn kind_sticks_until_lift() {
        let mut pad = Pad::new();
        pad.feed_all((0..20).map(|i| three((300.0 + 3.0 * i as f64, 300.0), 1.0, i)));
        assert_eq!(pad.g.kind, GestureKind::Pan);
        // Same fingers now pinch: still only a pan.
        let after = pad.feed_all((0..20).map(|i| three((357.0, 300.0), 1.0 + 0.04 * i as f64, 20 + i)));
        assert!(after.iter().flatten().all(|m| matches!(m, Motion::Pan(..))));
        assert_eq!(pad.g.kind, GestureKind::Pan);

        let mut pad = Pad::new();
        pad.feed_all((0..20).map(|i| three((300.0, 300.0), 1.0 + 0.03 * i as f64, i)));
        assert_eq!(pad.g.kind, GestureKind::Zoom);
        let after = pad.feed_all((0..20).map(|i| three((300.0 + 4.0 * i as f64, 300.0), 1.57, 20 + i)));
        assert!(after.iter().flatten().all(|m| matches!(m, Motion::Zoom(..))));
    }

    #[test]
    fn decision_applies_the_motion_before_it() {
        // Frame 4 is the last one still settling; everything after it counts.
        let frames: Vec<_> = (0..20).map(|i| three((300.0, 300.0), 1.0 + 0.03 * i as f64, i)).collect();
        let mut pad = Pad::new();
        let zoom: f64 = pad
            .feed_all(frames.clone())
            .iter()
            .flatten()
            .map(|m| match m {
                Motion::Zoom(f) => *f,
                Motion::Pan(..) => panic!("a pinch panned"),
            })
            .product();
        let s = |f: &[Contact]| spread(f, centroid(f));
        assert!((zoom - s(&frames[19]) / s(&frames[4])).abs() < 1e-9);

        let frames: Vec<_> = (0..20).map(|i| three((300.0 + 3.0 * i as f64, 300.0), 1.0, i)).collect();
        let mut pad = Pad::new();
        let (dx, dy) = pad.feed_all(frames.clone()).iter().flatten().fold((0.0, 0.0), |acc, m| match m {
            Motion::Pan(x, y) => (acc.0 + x, acc.1 + y),
            Motion::Zoom(_) => panic!("a drag zoomed"),
        });
        let (a, b) = (centroid(&frames[4]), centroid(&frames[19]));
        assert!((dx - (b.0 - a.0)).abs() < 1e-9 && (dy - (b.1 - a.1)).abs() < 1e-9);
    }

    #[test]
    fn a_lifting_finger_doesnt_move_the_camera() {
        let mut pad = Pad::new();
        pad.feed_all((0..20).map(|i| three((300.0 + 3.0 * i as f64, 300.0), 1.0, i)));
        assert_eq!(pad.g.kind, GestureKind::Pan);
        // One finger lifts; the other two drift apart as the hand relaxes.
        let two = |i: usize, d: f64| hand(&[(317.0 - d, 310.0), (397.0 + d, 305.0)], i);
        let held = pad.feed_all((0..8).map(|i| two(20 + i, i as f64)));
        assert!(held.iter().all(Option::is_none), "{held:?}");
        assert_eq!(pad.g.kind, GestureKind::Pan);
        let resumed = pad.feed(&two(28, 8.0));
        assert!(matches!(resumed, Some(Motion::Pan(..))));
    }

    #[test]
    fn only_a_moving_drag_flicks() {
        let mut pad = Pad::new();
        pad.feed_all((0..20).map(|i| three((300.0 + 6.0 * i as f64, 300.0), 1.0, i)));
        let v = pad.g.release_velocity(pad.now());
        assert!(v.0 > 100.0, "{v:?}");
        assert_eq!(pad.g.release_velocity(pad.now() + Duration::from_millis(200)), (0.0, 0.0));

        let mut pad = Pad::new();
        pad.feed_all((0..20).map(|i| three((300.0, 300.0), 1.0 + 0.03 * i as f64, i)));
        assert_eq!(pad.g.kind, GestureKind::Zoom);
        assert_eq!(pad.g.release_velocity(pad.now()), (0.0, 0.0));
    }

    #[test]
    fn a_jumpy_last_frame_barely_changes_the_flick() {
        let mut pad = Pad::new();
        pad.feed_all((0..20).map(|i| three((300.0 + 6.0 * i as f64, 300.0), 1.0, i)));
        let smooth = pad.g.release_velocity(pad.now());
        let mut pad = Pad::new();
        pad.feed_all((0..20).map(|i| three((300.0 + 6.0 * i as f64, 300.0), 1.0, i)));
        pad.feed(&three((300.0 + 6.0 * 20.0 + 60.0, 300.0), 1.0, 20));
        let jumpy = pad.g.release_velocity(pad.now());
        assert!(jumpy.0 < smooth.0 * 2.0, "{smooth:?} {jumpy:?}");
        assert!(jumpy.0.hypot(jumpy.1) <= MAX_FLICK + 1e-6);
    }

    #[test]
    fn a_tap_is_nothing() {
        let frames = (0..10).map(|i| three((300.0 + 0.3 * i as f64, 300.0), 1.0, i));
        assert_eq!(decide(frames), GestureKind::Undecided);
    }

    #[test]
    fn classify_thresholds() {
        let ev = |shared, travel, spread, log_ratio| Evidence { shared, travel, spread, log_ratio };
        // Small motion of either kind waits.
        assert_eq!(classify(&ev(5.0, 5.0, 5.0, 0.2)), GestureKind::Undecided);
        // A big spread change that is a small ratio isn't a pinch yet.
        assert_eq!(classify(&ev(0.0, 2.0, 7.0, 0.06)), GestureKind::Undecided);
        assert_eq!(classify(&ev(0.0, 5.0, 7.0, 0.1)), GestureKind::Zoom);
        assert_eq!(classify(&ev(7.0, 7.0, 3.0, 0.03)), GestureKind::Pan);
        // Splay and shared travel alike: wait, then the drag wins.
        assert_eq!(classify(&ev(5.0, 9.0, 8.0, 0.09)), GestureKind::Undecided);
        assert_eq!(classify(&ev(2.0, 19.0, 8.0, 0.06)), GestureKind::Pan);
    }

    #[test]
    fn wheel_burst_keeps_its_kind_until_quiet() {
        let t = Instant::now();
        let soon = t + Duration::from_millis(20);
        let later = t + Duration::from_millis(WHEEL_IDLE_MS as u64 + 1);
        assert!(wheel_accepts(None, true, t));
        assert!(wheel_accepts(Some((true, t)), true, soon));
        assert!(!wheel_accepts(Some((true, t)), false, soon));
        assert!(!wheel_accepts(Some((false, t)), true, soon));
        assert!(wheel_accepts(Some((false, t)), true, later));
    }
}
