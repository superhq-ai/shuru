//! The bit budget of the stream: a token bucket in wall time.
//!
//! It holds up to 400 ms of video at the target bitrate and refills at the
//! target, also while the screen is still. Each frame may take what the
//! bucket holds, so a change after a still moment (a tab switch, a
//! keyframe) comes out sharp at once, while during sustained motion the
//! bucket is empty and each frame gets its share of the target. Nothing
//! waits: the frame rate is never what gives. Paint-over, which sharpens
//! a screen gone still, waits for a full bucket so it can sharpen fully.
//!
//! x264 cannot keep this bucket itself. Without `b_vfr_input` it refills by
//! 1/fps for each frame encoded, and frames come only when the screen
//! changes; with it, it holds every frame back by one, which a damage
//! driven stream cannot afford. So x264 only caps each frame (see x264.c),
//! at the size this bucket says.
//!
//! How deep the bucket gets depends on the path. A deep burst only pays
//! off on a link that can carry it: on one no faster than the target it
//! just queues for the whole depth, so a fast flick stalls. The reach,
//! below, reads the measured headroom (the capacity the rate controller
//! found over the target) and the bucket is deep only once the path has
//! shown room for it.

use std::time::{Duration, Instant};

/// The deepest the bucket gets on a roomy path, as video at the target.
pub const ROOMY_MS: f64 = 400.0;

/// The deepest on a path with no room over the target: about one frame,
/// so a change is coded no bigger than the shipped encoder would, and no
/// deep queue builds.
pub const TIGHT_MS: f64 = 40.0;

/// Whether the path has shown room for a deep burst. A pure state machine
/// over the measured headroom (capacity over target), with a gap between
/// opening and closing so it does not flap on a brief overshoot.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Reach {
    /// No room measured over the target: keep the bucket about one frame.
    Tight,
    /// The path carried well over the target: allow the full depth.
    Roomy,
}

impl Reach {
    /// Reopens at this much measured headroom. A path whose capacity is
    /// unknown reads as infinite headroom, so an untried path is roomy.
    const OPEN: f64 = 1.5;

    /// A queue building now closes it at once, before the ceiling is even
    /// measured, so the first frames of a burst on a link no faster than
    /// the target do not run away. It reopens only once the measured
    /// headroom is clearly above the target and nothing is queued.
    pub fn step(self, congested: bool, headroom: f64) -> Reach {
        if congested {
            Reach::Tight
        } else if headroom >= Self::OPEN {
            Reach::Roomy
        } else {
            self
        }
    }

    pub fn depth_ms(self) -> f64 {
        match self {
            Reach::Tight => TIGHT_MS,
            Reach::Roomy => ROOMY_MS,
        }
    }
}

pub struct Budget {
    /// Kilobits in the bucket. Below 0 after a frame larger than its cap.
    level: f64,
    at: Instant,
    /// How deep the bucket may get, in ms of video at the target.
    depth_ms: f64,
}

impl Budget {
    /// A full bucket, roomy until the path says otherwise.
    pub fn new(now: Instant) -> Self {
        Budget {
            level: f64::INFINITY,
            at: now,
            depth_ms: ROOMY_MS,
        }
    }

    /// Sets how deep the bucket may get, from the path's reach.
    pub fn set_depth_ms(&mut self, depth_ms: f64) {
        self.depth_ms = depth_ms;
    }

    fn depth(&self, kbps: u32) -> f64 {
        kbps as f64 * self.depth_ms / 1000.0
    }

    fn refill(&mut self, now: Instant, kbps: u32) {
        let secs = now.saturating_duration_since(self.at).as_secs_f64();
        let depth = self.depth(kbps);
        self.level = (self.level + secs * kbps as f64).min(depth);
        self.at = now;
    }

    /// The most the next frame may take, in kbit: what the bucket holds,
    /// never less than one frame's share of the target at `fps`.
    pub fn frame_kbit(&mut self, now: Instant, kbps: u32, fps: u32) -> u32 {
        self.refill(now, kbps);
        let share = kbps as f64 / fps.max(1) as f64;
        self.level.max(share).max(1.0) as u32
    }

    /// How long until the bucket is full again.
    pub fn until_full(&mut self, now: Instant, kbps: u32) -> Duration {
        self.refill(now, kbps);
        let short = self.depth(kbps) - self.level;
        Duration::from_secs_f64((short / kbps.max(1) as f64).max(0.0))
    }

    /// A frame of `bytes` went out.
    pub fn spend(&mut self, bytes: usize, kbps: u32) {
        // A frame over its cap is owed, up to one bucket.
        let floor = -self.depth(kbps);
        self.level = (self.level - bytes as f64 * 8.0 / 1000.0).max(floor);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn a_change_after_a_still_moment_gets_the_whole_bucket() {
        let t0 = Instant::now();
        let mut b = Budget::new(t0);
        // 400 ms at 1500 kbit/s: 600 kbit, 75 kB.
        assert_eq!(b.frame_kbit(t0, 1500, 30), 600);
        b.spend(70_000, 1500);
        // 40 kbit left, under one frame's share of 50.
        assert_eq!(b.frame_kbit(t0, 1500, 30), 50);
        // Refilled at the target while still, up to the depth and no more.
        assert_eq!(b.frame_kbit(t0 + ms(200), 1500, 30), 340);
        assert_eq!(b.frame_kbit(t0 + ms(10_000), 1500, 30), 600);
    }

    #[test]
    fn sustained_motion_runs_on_the_share_of_each_frame() {
        let t0 = Instant::now();
        let mut b = Budget::new(t0);
        let mut t = t0;
        let mut sent = 0usize;
        // Frames that always take their whole cap, 33 ms apart, for 3 s.
        for _ in 0..90 {
            let cap = b.frame_kbit(t, 3000, 30);
            b.spend(cap as usize * 1000 / 8, 3000);
            sent += cap as usize * 1000 / 8;
            t += ms(33);
        }
        // The bucket, 150 kB, then the target, 375 kB/s, over 2.97 s.
        let most = 150_000.0 + 375_000.0 * 2.97;
        assert!(sent as f64 <= most * 1.02, "{sent} > {most}");
        // Late in the run each frame gets its share: 100 kbit.
        assert!((b.frame_kbit(t, 3000, 30) as i64 - 100).abs() <= 2);
    }

    #[test]
    fn an_overshoot_is_owed_but_motion_still_gets_its_share() {
        let t0 = Instant::now();
        let mut b = Budget::new(t0);
        b.frame_kbit(t0, 1500, 30);
        // Far over: owed down to one bucket below empty.
        b.spend(1_000_000, 1500);
        assert_eq!(b.frame_kbit(t0, 1500, 30), 50);
        // 400 ms pays the debt back, 400 more fills the bucket.
        assert_eq!(b.frame_kbit(t0 + ms(400), 1500, 30), 50);
        assert_eq!(b.frame_kbit(t0 + ms(800), 1500, 30), 600);
    }

    #[test]
    fn paint_over_knows_when_the_bucket_is_full() {
        let t0 = Instant::now();
        let mut b = Budget::new(t0);
        assert_eq!(b.until_full(t0, 1500), Duration::ZERO);
        b.frame_kbit(t0, 1500, 30);
        b.spend(75_000, 1500);
        let near = |d: Duration, want: u64| (d.as_secs_f64() * 1000.0 - want as f64).abs() < 0.01;
        assert!(near(b.until_full(t0, 1500), 400));
        assert!(near(b.until_full(t0 + ms(100), 1500), 300));
        assert_eq!(b.until_full(t0 + ms(401), 1500), Duration::ZERO);
    }

    #[test]
    fn a_lower_target_shrinks_the_bucket() {
        let t0 = Instant::now();
        let mut b = Budget::new(t0);
        assert_eq!(b.frame_kbit(t0, 6000, 30), 2400);
        assert_eq!(b.frame_kbit(t0, 1500, 30), 600);
    }

    #[test]
    fn reach_closes_on_a_queue_and_reopens_only_on_room() {
        // A queue closes it at once, whatever the measured headroom.
        assert_eq!(Reach::Roomy.step(true, f64::INFINITY), Reach::Tight);
        assert_eq!(Reach::Roomy.step(true, 3.0), Reach::Tight);
        // Tight holds while the path has no clear room over the target,
        // even with nothing queued, so a link at the target stays tight.
        assert_eq!(Reach::Tight.step(false, 1.0), Reach::Tight);
        assert_eq!(Reach::Tight.step(false, 1.4), Reach::Tight);
        // Half again the target's room, and clear, reopens it.
        assert_eq!(Reach::Tight.step(false, 1.5), Reach::Roomy);
        // An untried path reads as infinite headroom: roomy.
        assert_eq!(Reach::Tight.step(false, f64::INFINITY), Reach::Roomy);
    }

    #[test]
    fn a_tight_reach_caps_the_burst_near_one_frame() {
        let t0 = Instant::now();
        let mut b = Budget::new(t0);
        // Roomy by default: a change after rest gets the whole 400 ms.
        assert_eq!(b.frame_kbit(t0, 1500, 30), 600);
        // On a path with no room the bucket is about one frame, close to
        // the per-frame share, so a burst cannot run away.
        b.set_depth_ms(TIGHT_MS);
        assert_eq!(b.frame_kbit(t0, 1500, 30), 60);
        // And the debt from an overshoot is bounded at that shallow depth.
        b.spend(75_000, 1500);
        assert_eq!(b.frame_kbit(t0, 1500, 30), 50);
    }
}
