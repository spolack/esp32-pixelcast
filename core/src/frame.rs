//! When a received LED frame is shown: the rules on top of the universe
//! data (which the firmware copies into the frame itself).
//!
//! - A frame is shown once every universe it maps has arrived.
//! - A universe the frame already has means the next frame has begun (a
//!   packet of this one was lost): the frame is shown first, without
//!   waiting for the timeout.
//! - An incomplete frame is shown [`FRAME_TIMEOUT_US`] after its first
//!   universe.
//! - In sync mode (an ArtSync within [`SYNC_TIMEOUT_US`]) only ArtSync shows
//!   frames; a sender that stops sending ArtSync falls back after that time.
//!
//! Times are microseconds since start.

/// An incomplete frame is shown this long after its first universe.
pub const FRAME_TIMEOUT_US: u64 = 30_000;
/// Sync mode lasts this long after the last ArtSync.
pub const SYNC_TIMEOUT_US: u64 = 2_000_000;

/// The state of the frame being received.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Gate {
    /// Universe bits (of the mapping's slots) received so far.
    received: u64,
    first_at: Option<u64>,
    last_sync: Option<u64>,
    /// Bumped whenever a frame is shown.
    seq: u32,
}

impl Default for Gate {
    fn default() -> Self {
        Self::new()
    }
}

impl Gate {
    pub const fn new() -> Self {
        Self { received: 0, first_at: None, last_sync: None, seq: 0 }
    }

    /// Bumped whenever a frame is shown; the LED side compares it.
    // Inlined into the callers, which run from IRAM.
    #[inline(always)]
    pub fn seq(&self) -> u32 {
        self.seq
    }

    #[inline(always)]
    fn sync_mode(&self, now: u64) -> bool {
        self.last_sync.is_some_and(|t| now.saturating_sub(t) < SYNC_TIMEOUT_US)
    }

    #[inline(always)]
    fn show(&mut self) -> bool {
        self.received = 0;
        self.first_at = None;
        self.seq = self.seq.wrapping_add(1);
        true
    }

    /// Before a universe with slot bits `hit` is copied into the frame. True:
    /// the frame already had it and was shown first (the next one begins).
    #[inline(always)]
    pub fn before_universe(&mut self, hit: u64, now: u64) -> bool {
        self.received & hit != 0 && !self.sync_mode(now) && self.show()
    }

    /// After it was copied; `all` are the bits of a complete frame. True:
    /// the frame is complete and shown.
    #[inline(always)]
    pub fn after_universe(&mut self, hit: u64, all: u64, now: u64) -> bool {
        self.received |= hit;
        self.first_at.get_or_insert(now);
        !self.sync_mode(now) && self.received & all == all && self.show()
    }

    /// An ArtSync: sync mode, and the frame is shown if `show` (something is
    /// mapped at all).
    #[inline(always)]
    pub fn sync(&mut self, now: u64, show: bool) -> bool {
        self.last_sync = Some(now);
        show && self.show()
    }

    /// Call now and then: true if an incomplete frame's time is up and it was
    /// shown.
    #[inline(always)]
    pub fn tick(&mut self, now: u64) -> bool {
        let late = self.first_at.is_some_and(|t| now.saturating_sub(t) >= FRAME_TIMEOUT_US);
        late && !self.sync_mode(now) && self.show()
    }

    /// Drops what was received (the layout changed). Sync mode stays.
    #[inline(always)]
    pub fn restart(&mut self) {
        self.received = 0;
        self.first_at = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: u64 = 0b111;

    /// Feeds universe bit `b` at `t` ms; true if a frame was shown.
    fn feed(g: &mut Gate, b: u32, t: u64) -> bool {
        let before = g.before_universe(1 << b, t * 1000);
        let after = g.after_universe(1 << b, ALL, t * 1000);
        before || after
    }

    #[test]
    fn complete_frame_shows_once() {
        let mut g = Gate::new();
        assert!(!feed(&mut g, 0, 0));
        assert!(!feed(&mut g, 1, 1));
        assert!(feed(&mut g, 2, 2));
        assert_eq!(g.seq(), 1);
        assert!(!g.tick(100_000), "nothing pending after showing");
    }

    #[test]
    fn repeated_universe_shows_the_partial_frame() {
        let mut g = Gate::new();
        feed(&mut g, 0, 0);
        feed(&mut g, 1, 1);
        // Universe 2 was lost; universe 0 of the next frame arrives.
        assert!(g.before_universe(1, 20_000));
        assert_eq!(g.seq(), 1);
        assert!(!g.after_universe(1, ALL, 20_000), "the new frame has only universe 0");
    }

    #[test]
    fn timeout_shows_incomplete_frame() {
        let mut g = Gate::new();
        feed(&mut g, 0, 10);
        assert!(!g.tick(10_000 + FRAME_TIMEOUT_US - 1));
        assert!(g.tick(10_000 + FRAME_TIMEOUT_US));
        assert!(!g.tick(10_000 + 2 * FRAME_TIMEOUT_US), "shown only once");
        assert!(!Gate::new().tick(u64::MAX), "an empty frame never times out");
    }

    #[test]
    fn sync_mode_waits_for_sync() {
        let mut g = Gate::new();
        assert!(g.sync(0, true), "a sync shows the frame");
        assert!(!feed(&mut g, 0, 1));
        assert!(!feed(&mut g, 1, 1));
        assert!(!feed(&mut g, 2, 1), "complete, but sync mode waits");
        assert!(!feed(&mut g, 0, 2), "a repeat does not show either");
        assert!(!g.tick(1_000_000), "no timeout in sync mode");
        assert!(g.sync(1_000_000, true));
        assert_eq!(g.seq(), 2);
        assert!(!g.sync(1_000_001, false), "nothing mapped: nothing shown");
    }

    #[test]
    fn sync_mode_expires() {
        let mut g = Gate::new();
        g.sync(0, true);
        feed(&mut g, 0, 1);
        assert!(!g.tick(SYNC_TIMEOUT_US - 1));
        assert!(g.tick(SYNC_TIMEOUT_US), "back to timeouts once ArtSync stops");
        feed(&mut g, 0, 2_100);
        feed(&mut g, 1, 2_100);
        assert!(feed(&mut g, 2, 2_100), "and complete frames show again");
    }

    #[test]
    fn restart_drops_partial_frame() {
        let mut g = Gate::new();
        feed(&mut g, 0, 0);
        g.restart();
        assert!(!g.tick(1_000_000));
        assert!(!g.before_universe(1, 1_000_000), "universe 0 is new again");
        assert_eq!(g.seq(), 0);
    }

    #[test]
    fn duplicate_bits_from_one_universe() {
        // One universe feeding two outputs sets two bits at once.
        let mut g = Gate::new();
        assert!(!g.after_universe(0b011, ALL, 0));
        assert!(g.after_universe(0b100, ALL, 1));
    }
}
