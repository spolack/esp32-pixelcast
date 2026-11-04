//! Art-Net merging: two sources (consoles) sending the same universe.
//!
//! As the Art-Net 4 spec describes it: a node merges at most two sources
//! per universe, told apart by IP address; a third is ignored. A source that
//! has sent nothing for 10 s drops out. HTP (highest value per channel wins)
//! is the spec's default; LTP (the latest packet wins) can be chosen in the
//! settings or by ArtAddress (AcMergeLtp/AcMergeHtp). AcCancelMerge makes
//! the next source that sends the only one.
//!
//! With a single source the data passes through untouched; nothing is
//! stored. Per-source buffers (512 bytes) exist only while two sources
//! merge in HTP.

use alloc::vec::Vec;

/// A source drops out after this long without a packet.
pub const TIMEOUT_MS: u64 = 10_000;
/// Universes tracked (more than the outputs can use).
const UNIVERSES: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Htp,
    Ltp,
}

impl Mode {
    pub fn key(self) -> &'static str {
        match self {
            Mode::Htp => "htp",
            Mode::Ltp => "ltp",
        }
    }
    pub fn parse(s: &str) -> Option<Mode> {
        match s {
            "htp" => Some(Mode::Htp),
            "ltp" => Some(Mode::Ltp),
            _ => None,
        }
    }
}

struct Source {
    ip: [u8; 4],
    last_ms: u64,
    /// The latest data, kept only while merging in HTP.
    data: Vec<u8>,
}

struct Universe {
    universe: u16,
    src: [Option<Source>; 2],
}

pub struct Merger {
    unis: Vec<Universe>,
    out: [u8; 512],
    cancel: bool,
}

impl Default for Merger {
    fn default() -> Self {
        Self::new()
    }
}

impl Merger {
    pub const fn new() -> Self {
        Self { unis: Vec::new(), out: [0; 512], cancel: false }
    }

    /// AcCancelMerge: the next packet's source becomes the only one.
    pub fn cancel(&mut self) {
        self.cancel = true;
    }

    /// One ArtDmx packet. Returns the data to show, or `None` when the
    /// packet is to be ignored (a third source).
    #[cfg_attr(target_arch = "xtensa", esp_hal::ram)]
    pub fn feed<'a>(&'a mut self, universe: u16, ip: [u8; 4], now_ms: u64, data: &'a [u8], mode: Mode) -> Option<&'a [u8]> {
        let idx = match self.unis.iter().position(|u| u.universe == universe) {
            Some(i) => i,
            None if self.unis.len() < UNIVERSES => {
                self.unis.push(Universe { universe, src: [None, None] });
                self.unis.len() - 1
            }
            None => return Some(data),
        };
        let u = &mut self.unis[idx];
        for s in u.src.iter_mut() {
            if s.as_ref().is_some_and(|s| now_ms.saturating_sub(s.last_ms) > TIMEOUT_MS) {
                *s = None;
            }
        }
        if self.cancel {
            self.cancel = false;
            for un in self.unis.iter_mut() {
                for s in un.src.iter_mut() {
                    if s.as_ref().is_some_and(|s| s.ip != ip) {
                        *s = None;
                    }
                }
            }
        }
        let u = &mut self.unis[idx];
        let me = match u.src.iter().position(|s| s.as_ref().is_some_and(|s| s.ip == ip)) {
            Some(i) => i,
            None => match u.src.iter().position(Option::is_none) {
                Some(i) => {
                    u.src[i] = Some(Source { ip, last_ms: now_ms, data: Vec::new() });
                    i
                }
                None => return None, // a third source
            },
        };
        let other = 1 - me;
        if let Some(s) = u.src[me].as_mut() {
            s.last_ms = now_ms;
        }
        if u.src[other].is_none() || mode == Mode::Ltp {
            // One source, or LTP: the packet as it is. Drop stored copies.
            for s in u.src.iter_mut().flatten() {
                s.data = Vec::new();
            }
            return Some(data);
        }
        // HTP with two sources: keep this one's data, output the maximum.
        // Fallible: with the setup Wi-Fi on the heap is small, and a failed
        // allocation would panic. Without room, this packet goes out as is
        // (LTP) and the copies are dropped.
        if let Some(s) = u.src[me].as_mut() {
            let n = data.len().min(512);
            s.data.clear();
            if s.data.try_reserve_exact(n).is_err() {
                for s in u.src.iter_mut().flatten() {
                    s.data = Vec::new();
                }
                return Some(data);
            }
            s.data.extend_from_slice(&data[..n]);
        }
        let a = u.src[0].as_ref().map_or(&[][..], |s| &s.data[..]);
        let b = u.src[1].as_ref().map_or(&[][..], |s| &s.data[..]);
        let n = a.len().max(b.len());
        for (i, o) in self.out[..n].iter_mut().enumerate() {
            *o = a.get(i).copied().unwrap_or(0).max(b.get(i).copied().unwrap_or(0));
        }
        Some(&self.out[..n])
    }

    /// Whether `universe` has two live sources.
    pub fn merging(&self, universe: u16, now_ms: u64) -> bool {
        self.unis.iter().any(|u| {
            u.universe == universe
                && u.src.iter().all(|s| s.as_ref().is_some_and(|s| now_ms.saturating_sub(s.last_ms) <= TIMEOUT_MS))
        })
    }

    /// Writes the universes with two live sources into `out`; returns how many.
    pub fn merging_list(&self, now_ms: u64, out: &mut [u16]) -> usize {
        let mut n = 0;
        for u in self.unis.iter().filter(|u| self.merging(u.universe, now_ms)) {
            if let Some(o) = out.get_mut(n) {
                *o = u.universe;
                n += 1;
            }
        }
        n
    }

    /// Universes with two live sources.
    pub fn merging_count(&self, now_ms: u64) -> usize {
        self.unis.iter().filter(|u| self.merging(u.universe, now_ms)).count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const A: [u8; 4] = [10, 0, 0, 1];
    const B: [u8; 4] = [10, 0, 0, 2];
    const C: [u8; 4] = [10, 0, 0, 3];

    #[test]
    fn single_source_passes_through() {
        let mut m = Merger::new();
        assert_eq!(m.feed(1, A, 0, &[5, 6], Mode::Htp), Some(&[5u8, 6][..]));
        assert!(!m.merging(1, 0));
    }

    #[test]
    fn htp_takes_the_maximum_and_third_is_ignored() {
        let mut m = Merger::new();
        m.feed(1, A, 0, &[10, 200, 0], Mode::Htp);
        // B arrives: A's data was not kept while alone, so B's alone counts once.
        assert_eq!(m.feed(1, B, 10, &[50, 50], Mode::Htp), Some(&[50u8, 50][..]));
        assert_eq!(m.feed(1, A, 20, &[10, 200, 0], Mode::Htp), Some(&[50u8, 200, 0][..]));
        assert!(m.merging(1, 20));
        assert_eq!(m.merging_count(20), 1);
        assert_eq!(m.feed(1, C, 30, &[255], Mode::Htp), None);
        // Other universes are separate.
        assert_eq!(m.feed(2, C, 30, &[1], Mode::Htp), Some(&[1u8][..]));
    }

    #[test]
    fn ltp_takes_the_latest() {
        let mut m = Merger::new();
        m.feed(1, A, 0, &[10, 10], Mode::Ltp);
        assert_eq!(m.feed(1, B, 1, &[0, 5], Mode::Ltp), Some(&[0u8, 5][..]));
        assert_eq!(m.feed(1, A, 2, &[10, 10], Mode::Ltp), Some(&[10u8, 10][..]));
        assert!(m.merging(1, 2));
    }

    #[test]
    fn silent_source_times_out_and_cancel_drops_the_other() {
        let mut m = Merger::new();
        m.feed(1, A, 0, &[1], Mode::Htp);
        m.feed(1, B, 0, &[2], Mode::Htp);
        // A silent for more than 10 s: C takes its place.
        m.feed(1, B, 9_000, &[2], Mode::Htp);
        assert_eq!(m.feed(1, C, 10_500, &[3], Mode::Htp), Some(&[3u8][..]));
        assert!(m.merging(1, 10_500));
        m.cancel();
        assert_eq!(m.feed(1, B, 10_600, &[2], Mode::Htp), Some(&[2u8][..]));
        assert!(!m.merging(1, 10_600));
    }
}
