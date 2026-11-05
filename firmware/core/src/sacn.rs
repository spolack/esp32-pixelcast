//! sACN (ANSI E1.31) packets.
//!
//! Parsing what a receiving node needs (data packets, DMX512 with start
//! code 0, and synchronization packets) and building data packets. sACN numbers universes from 1; this firmware
//! uses Art-Net's port-address (from 0) everywhere, so sACN universe `u`
//! feeds port-address `u - 1` (the usual convention: sACN 1 = Art-Net 0).

pub const PORT: u16 = 5568;

const ACN_ID: &[u8; 12] = b"ASC-E1.17\0\0\0";
const VECTOR_ROOT_DATA: u32 = 0x0000_0004;
const VECTOR_ROOT_EXTENDED: u32 = 0x0000_0008;
const VECTOR_FRAMING_DATA: u32 = 0x0000_0002;
const VECTOR_EXTENDED_SYNC: u32 = 0x0000_0001;
const VECTOR_DMP_SET_PROPERTY: u8 = 0x02;
/// Data packet: start code (offset 125), then the slots.
const DATA_START: usize = 126;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Packet<'a> {
    /// DMX data. `universe` is the sACN universe (1..=63999).
    Data {
        universe: u16,
        priority: u8,
        sequence: u8,
        /// Synchronization universe (0 = none): the data waits for a sync.
        sync_universe: u16,
        /// Preview data, not meant for live output.
        preview: bool,
        /// The source stopped sending this universe.
        terminated: bool,
        data: &'a [u8],
    },
    /// Synchronization packet for `sync_universe`.
    Sync { sync_universe: u16 },
}

/// Length of a data packet with 512 slots.
pub const DATA_PACKET_LEN: usize = DATA_START + 512;

/// Writes a data packet for sACN `universe` (1..=63999) into `p`: 512
/// slots, `slots` first and the rest 0. `cid` is the source's component id,
/// `name` its source name (up to 63 bytes). Returns its length
/// ([`DATA_PACKET_LEN`]); `p` must hold it.
pub fn write_data(
    p: &mut [u8],
    cid: &[u8; 16],
    name: &str,
    priority: u8,
    sequence: u8,
    universe: u16,
    slots: &[u8],
) -> usize {
    // Flags 0x7 and the length of the PDU from `at` to the end.
    fn flags_len(p: &mut [u8], at: usize) {
        let len = (DATA_PACKET_LEN - at) as u16;
        p[at..at + 2].copy_from_slice(&(0x7000 | len).to_be_bytes());
    }
    let p = &mut p[..DATA_PACKET_LEN];
    p.fill(0);
    p[0..2].copy_from_slice(&0x0010u16.to_be_bytes());
    p[4..16].copy_from_slice(ACN_ID);
    flags_len(p, 16);
    p[18..22].copy_from_slice(&VECTOR_ROOT_DATA.to_be_bytes());
    p[22..38].copy_from_slice(cid);
    flags_len(p, 38);
    p[40..44].copy_from_slice(&VECTOR_FRAMING_DATA.to_be_bytes());
    let n = name.len().min(63);
    p[44..44 + n].copy_from_slice(&name.as_bytes()[..n]);
    p[108] = priority;
    p[111] = sequence;
    p[113..115].copy_from_slice(&universe.to_be_bytes());
    flags_len(p, 115);
    p[117] = VECTOR_DMP_SET_PROPERTY;
    p[118] = 0xa1;
    p[121..123].copy_from_slice(&1u16.to_be_bytes());
    p[123..125].copy_from_slice(&513u16.to_be_bytes());
    let n = slots.len().min(512);
    p[DATA_START..DATA_START + n].copy_from_slice(&slots[..n]);
    DATA_PACKET_LEN
}

fn be16(p: &[u8], at: usize) -> u16 {
    u16::from_be_bytes([p[at], p[at + 1]])
}

fn be32(p: &[u8], at: usize) -> u32 {
    u32::from_be_bytes([p[at], p[at + 1], p[at + 2], p[at + 3]])
}

/// Parses one UDP payload; `None` for anything that is not a usable E1.31
/// data or sync packet (including non-zero start codes such as RDM).
pub fn parse(p: &[u8]) -> Option<Packet<'_>> {
    // Root layer: preamble size 0x0010, postamble 0, ACN packet identifier.
    if p.len() < 49 || be16(p, 0) != 0x0010 || be16(p, 2) != 0 || &p[4..16] != ACN_ID {
        return None;
    }
    match be32(p, 18) {
        VECTOR_ROOT_DATA => {
            if p.len() < DATA_START || be32(p, 40) != VECTOR_FRAMING_DATA {
                return None;
            }
            // DMP layer: set property, address type 0xa1, first address 0,
            // increment 1, then the property value count (start code + slots).
            if p[117] != VECTOR_DMP_SET_PROPERTY || p[118] != 0xa1 || be16(p, 119) != 0 || be16(p, 121) != 1 {
                return None;
            }
            let count = usize::from(be16(p, 123));
            if !(1..=513).contains(&count) || p.len() < DATA_START - 1 + count || p[125] != 0 {
                return None;
            }
            let universe = be16(p, 113);
            if universe == 0 || universe > 63999 {
                return None;
            }
            let options = p[112];
            Some(Packet::Data {
                universe,
                priority: p[108],
                sequence: p[111],
                sync_universe: be16(p, 109),
                preview: options & 0x80 != 0,
                terminated: options & 0x40 != 0,
                data: &p[DATA_START..DATA_START - 1 + count],
            })
        }
        VECTOR_ROOT_EXTENDED => {
            if be32(p, 40) != VECTOR_EXTENDED_SYNC {
                return None;
            }
            Some(Packet::Sync { sync_universe: be16(p, 45) })
        }
        _ => None,
    }
}

/// Multicast group of a sACN universe: 239.255.hi.lo.
pub fn multicast_group(universe: u16) -> [u8; 4] {
    let [hi, lo] = universe.to_be_bytes();
    [239, 255, hi, lo]
}

/// The port-address (Art-Net numbering, from 0) that sACN `universe` feeds;
/// `None` for universes Art-Net cannot address (0, above 32768), which
/// would otherwise alias onto low ones.
pub fn port_address(universe: u16) -> Option<u16> {
    (1..=0x8000).contains(&universe).then(|| universe - 1)
}

/// Source priority per universe: a source with a lower priority than one
/// heard in the last [`Priorities::HOLD_MS`] is ignored (E1.31's rule; equal
/// priorities mix, the last packet wins).
pub struct Priorities {
    /// (universe, priority, last seen in ms, wrapping; 0 = free). 64 covers
    /// what the outputs map in practice; beyond it filtering is off for the
    /// extra universes (everything is accepted).
    seen: [(u16, u8, u32); 64],
}

impl Default for Priorities {
    fn default() -> Self {
        Self::new()
    }
}

impl Priorities {
    pub const HOLD_MS: u32 = 2500;

    pub const fn new() -> Self {
        Self { seen: [(0, 0, 0); 64] }
    }

    /// A source sent "stream terminated": if it is the one holding
    /// `universe`, lower priorities may take over at once.
    pub fn release(&mut self, universe: u16, priority: u8) {
        for e in self.seen.iter_mut() {
            if e.0 == universe && e.2 != 0 && e.1 == priority {
                *e = (0, 0, 0);
            }
        }
    }

    /// Whether to take a packet on `universe` with `priority` at `now_ms`
    /// (milliseconds, may wrap).
    pub fn accept(&mut self, universe: u16, priority: u8, now_ms: u32) -> bool {
        let now = now_ms.max(1);
        let mut free = None;
        for (i, e) in self.seen.iter_mut().enumerate() {
            let expired = now.wrapping_sub(e.2) > Self::HOLD_MS;
            if e.0 == universe && e.2 != 0 {
                if expired || priority >= e.1 {
                    *e = (universe, priority, now);
                    return true;
                }
                return false;
            }
            if free.is_none() && (e.2 == 0 || expired) {
                free = Some(i);
            }
        }
        if let Some(i) = free {
            self.seen[i] = (universe, priority, now);
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn priorities() {
        let mut p = Priorities::new();
        assert!(p.accept(1, 100, 1000));
        assert!(!p.accept(1, 50, 1100), "lower priority ignored");
        assert!(p.accept(1, 100, 1200), "equal mixes");
        assert!(p.accept(2, 50, 1200), "other universe independent");
        assert!(!p.accept(1, 50, 1200 + Priorities::HOLD_MS));
        assert!(p.accept(1, 50, 1201 + Priorities::HOLD_MS), "high source gone");
        assert!(!p.accept(1, 10, 1300 + Priorities::HOLD_MS), "the low one now holds");
        // Across the millisecond counter's wrap.
        let mut w = Priorities::new();
        assert!(w.accept(7, 200, u32::MAX - 10));
        assert!(!w.accept(7, 100, 20));
        assert!(w.accept(7, 100, Priorities::HOLD_MS + 10));
        // 64 universes are tracked; more are accepted without filtering.
        let mut f = Priorities::new();
        for u in 0..64 {
            assert!(f.accept(u, 200, 5));
        }
        assert!(!f.accept(63, 1, 6));
        assert!(f.accept(64, 200, 6) && f.accept(64, 1, 6), "untracked: accepted");
        // Termination of the holding source lets a lower one in at once.
        let mut t = Priorities::new();
        assert!(t.accept(3, 200, 10));
        t.release(3, 100);
        assert!(!t.accept(3, 100, 20), "another priority's termination changes nothing");
        t.release(3, 200);
        assert!(t.accept(3, 100, 30));
        assert_eq!(port_address(1), Some(0));
        assert_eq!(port_address(0x8000), Some(0x7fff));
        assert_eq!(port_address(0x8001), None);
        assert_eq!(port_address(0), None);
    }
    extern crate std;
    use std::vec::Vec;

    fn data_packet(universe: u16, priority: u8, options: u8, slots: &[u8]) -> Vec<u8> {
        let mut p = std::vec![0u8; DATA_START + slots.len()];
        p[0..2].copy_from_slice(&0x0010u16.to_be_bytes());
        p[4..16].copy_from_slice(ACN_ID);
        p[18..22].copy_from_slice(&VECTOR_ROOT_DATA.to_be_bytes());
        p[40..44].copy_from_slice(&VECTOR_FRAMING_DATA.to_be_bytes());
        p[108] = priority;
        p[111] = 7;
        p[112] = options;
        p[113..115].copy_from_slice(&universe.to_be_bytes());
        p[117] = VECTOR_DMP_SET_PROPERTY;
        p[118] = 0xa1;
        p[121..123].copy_from_slice(&1u16.to_be_bytes());
        p[123..125].copy_from_slice(&((slots.len() + 1) as u16).to_be_bytes());
        p[DATA_START..].copy_from_slice(slots);
        p
    }

    #[test]
    fn built_packet_parses() {
        let mut p = [0xAAu8; DATA_PACKET_LEN];
        assert_eq!(write_data(&mut p, &[1; 16], "pixelcast audio", 100, 9, 17, &[3]), DATA_PACKET_LEN);
        let mut data = [0u8; 512];
        data[0] = 3;
        assert_eq!(&p[44..59], b"pixelcast audio");
        assert_eq!(be16(&p, 16) & 0x0fff, (DATA_PACKET_LEN - 16) as u16);
        assert_eq!(
            parse(&p),
            Some(Packet::Data {
                universe: 17,
                priority: 100,
                sequence: 9,
                sync_universe: 0,
                preview: false,
                terminated: false,
                data: &data[..],
            })
        );
    }

    #[test]
    fn data_packet_parses() {
        let p = data_packet(5, 120, 0, &[1, 2, 3]);
        assert_eq!(
            parse(&p),
            Some(Packet::Data {
                universe: 5,
                priority: 120,
                sequence: 7,
                sync_universe: 0,
                preview: false,
                terminated: false,
                data: &[1, 2, 3],
            })
        );
        assert_eq!(port_address(5), Some(4));
        assert_eq!(multicast_group(0x0102), [239, 255, 1, 2]);
    }

    #[test]
    fn rejects_bad_packets() {
        let mut p = data_packet(1, 100, 0, &[0; 512]);
        p[125] = 0xcc; // RDM start code
        assert_eq!(parse(&p), None);
        assert_eq!(parse(&data_packet(0, 100, 0, &[1])), None); // universe 0 is invalid
        let mut short = data_packet(1, 100, 0, &[1, 2]);
        short.truncate(120);
        assert_eq!(parse(&short), None);
        let p = data_packet(1, 100, 0x40, &[1]);
        assert!(matches!(parse(&p), Some(Packet::Data { terminated: true, .. })));
    }

    #[test]
    fn sync_packet_parses() {
        let mut p = std::vec![0u8; 49];
        p[0..2].copy_from_slice(&0x0010u16.to_be_bytes());
        p[4..16].copy_from_slice(ACN_ID);
        p[18..22].copy_from_slice(&VECTOR_ROOT_EXTENDED.to_be_bytes());
        p[40..44].copy_from_slice(&VECTOR_EXTENDED_SYNC.to_be_bytes());
        p[45..47].copy_from_slice(&7u16.to_be_bytes());
        assert_eq!(parse(&p), Some(Packet::Sync { sync_universe: 7 }));
    }
}
