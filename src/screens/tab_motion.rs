//! Layout transitions keyed by stable tab ID, independent of PTY lifetime.
use std::{collections::HashMap, time::{Duration, Instant}};

pub(super) const DURATION: Duration = Duration::from_millis(210);
pub(super) const GAP: f32 = 8.0;

pub(super) fn ease(progress: f32) -> f32 {
    1.0 - (1.0 - progress.clamp(0.0, 1.0)).powi(3)
}

struct Transition {
    from: f32,
    to: f32,
    started: Instant,
}

impl Transition {
    fn scale(&self, now: Instant) -> f32 {
        let progress = now.saturating_duration_since(self.started).as_secs_f32() / DURATION.as_secs_f32();
        self.from + (self.to - self.from) * ease(progress)
    }
    fn finished(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.started) >= DURATION
    }
}

#[derive(Default)]
pub(super) struct TabMotion {
    widths: HashMap<u64, f32>,
    transitions: HashMap<u64, Transition>,
}

impl TabMotion {
    pub(super) fn record_width(&mut self, id: u64, width: f32) {
        if width.is_finite() && width > 0.0 { self.widths.insert(id, width); }
    }
    pub(super) fn width(&self, id: u64) -> f32 { self.widths.get(&id).copied().unwrap_or(132.0) }
    pub(super) fn scale(&self, id: u64, now: Instant) -> f32 {
        self.transitions.get(&id).map_or(1.0, |motion| motion.scale(now))
    }
    pub(super) fn opening(&self, id: u64) -> bool {
        self.transitions.get(&id).is_some_and(|motion| motion.to == 1.0)
    }
    pub(super) fn closing(&self, id: u64) -> bool {
        self.transitions.get(&id).is_some_and(|motion| motion.to == 0.0)
    }
    pub(super) fn moving(&self, id: u64) -> bool { self.transitions.contains_key(&id) }
    pub(super) fn any(&self) -> bool { !self.transitions.is_empty() }
    pub(super) fn open(&mut self, id: u64, now: Instant) {
        self.transitions.insert(id, Transition { from: 0.0, to: 1.0, started: now });
    }
    pub(super) fn close(&mut self, id: u64, now: Instant) {
        if self.closing(id) { return; }
        let from = self.scale(id, now);
        self.transitions.insert(id, Transition { from, to: 0.0, started: now });
    }
    pub(super) fn remove(&mut self, id: u64) {
        self.widths.remove(&id);
        self.transitions.remove(&id);
    }
    /// End transitions on a UI timer, never during a paint callback.
    pub(super) fn advance(&mut self, now: Instant) -> Vec<u64> {
        let mut closed = Vec::new();
        self.transitions.retain(|id, motion| {
            if !motion.finished(now) { return true; }
            if motion.to == 0.0 { closed.push(*id); }
            false
        });
        closed.sort_unstable();
        closed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn opening_and_closing_change_the_real_slot_and_gap() {
        let now = Instant::now();
        let mut motion = TabMotion::default();
        motion.record_width(1, 196.0);
        motion.open(1, now);
        assert_eq!(motion.scale(1, now), 0.0);
        let mid = motion.scale(1, now + DURATION / 2);
        assert!(mid > 0.0 && mid < 1.0);
        assert!((motion.width(1) + GAP) * mid < 204.0);
        motion.advance(now + DURATION);
        motion.close(1, now + DURATION);
        assert_eq!(motion.scale(1, now + DURATION), 1.0);
        assert_eq!(motion.scale(1, now + DURATION * 2), 0.0);
        assert_eq!(motion.advance(now + DURATION * 2), vec![1]);
    }
    #[test]
    fn simultaneous_closes_keep_their_ids_and_widths() {
        let now = Instant::now();
        let mut motion = TabMotion::default();
        motion.record_width(7, 204.0);
        motion.record_width(9, 151.0);
        motion.close(7, now);
        motion.close(9, now);
        motion.close(7, now + DURATION / 2); // repeated click cannot restart it
        assert_eq!(motion.width(7), 204.0);
        assert_eq!(motion.width(9), 151.0);
        assert_eq!(motion.advance(now + DURATION), vec![7, 9]);
        assert!(!motion.any());
    }
    #[test]
    fn closing_during_open_does_not_jump_to_full_width() {
        let now = Instant::now();
        let mut motion = TabMotion::default();
        motion.open(3, now);
        let interrupted = now + DURATION / 4;
        let before = motion.scale(3, interrupted);
        motion.close(3, interrupted);
        assert_eq!(motion.scale(3, interrupted), before);
        assert_eq!(motion.advance(interrupted + DURATION), vec![3]);
    }
}
