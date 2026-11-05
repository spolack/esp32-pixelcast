//! Art-Net packet parsing and receive statistics.

use core::fmt;

pub const PORT: u16 = 6454;

/// Channels (bytes) in one DMX universe.
pub const DMX_CHANNELS: usize = 512;

/// An Ethernet frame carrying UDP to [`PORT`]: the source address and the
/// UDP payload. For the driver's fast path, so only the plain case: IPv4
/// without options, not fragmented, to `mac` or broadcast. Anything else is
/// `None` and goes to the network stack.
#[cfg_attr(target_arch = "xtensa", esp_hal::ram)]
pub fn udp_payload<'a>(frame: &'a [u8], mac: &[u8; 6]) -> Option<([u8; 4], &'a [u8])> {
    // Ethernet II
    const ETH_DST: core::ops::Range<usize> = 0..6;
    const ETH_TYPE: core::ops::Range<usize> = 12..14;
    const ETH_HEADER: usize = 14;
    const ETHERTYPE_IPV4: [u8; 2] = [0x08, 0x00];
    // IPv4, relative to its start
    const IP_VER_IHL: usize = 0;
    const IP_TOTAL_LEN: usize = 2;
    const IP_FLAGS_FRAG: usize = 6;
    const IP_PROTO: usize = 9;
    const IP_SRC: core::ops::Range<usize> = 12..16;
    const IP_HEADER: usize = 20;
    const V4_NO_OPTIONS: u8 = 0x45;
    const PROTO_UDP: u8 = 17;
    // UDP, relative to its start
    const UDP_DST_PORT: usize = 2;
    const UDP_LEN: usize = 4;
    const UDP_HEADER: usize = 8;

    let be16 = |b: &[u8], at: usize| usize::from(u16::from_be_bytes([b[at], b[at + 1]]));
    if frame.len() < ETH_HEADER + IP_HEADER + UDP_HEADER {
        return None;
    }
    let dst = &frame[ETH_DST];
    if dst != mac && dst != [0xff; 6] || frame[ETH_TYPE] != ETHERTYPE_IPV4 {
        return None;
    }
    let ip = &frame[ETH_HEADER..];
    // Not fragmented: no more-fragments flag, offset 0 (DF allowed).
    let fragmented = ip[IP_FLAGS_FRAG] & 0x3f != 0 || ip[IP_FLAGS_FRAG + 1] != 0;
    if ip[IP_VER_IHL] != V4_NO_OPTIONS || ip[IP_PROTO] != PROTO_UDP || fragmented {
        return None;
    }
    let ip_len = be16(ip, IP_TOTAL_LEN);
    if ip_len < IP_HEADER + UDP_HEADER || ip_len > ip.len() {
        return None;
    }
    let src: [u8; 4] = [ip[IP_SRC.start], ip[IP_SRC.start + 1], ip[IP_SRC.start + 2], ip[IP_SRC.start + 3]];
    let udp = &ip[IP_HEADER..ip_len];
    let udp_len = be16(udp, UDP_LEN);
    if be16(udp, UDP_DST_PORT) != usize::from(PORT) || udp_len < UDP_HEADER || udp_len > udp.len() {
        return None;
    }
    Some((src, &udp[UDP_HEADER..udp_len]))
}

const ID: &[u8; 8] = b"Art-Net\0";
const OP_POLL: u16 = 0x2000;
const OP_POLL_REPLY: u16 = 0x2100;
const OP_ADDRESS: u16 = 0x6000;
const OP_DMX: u16 = 0x5000;
const OP_SYNC: u16 = 0x5200;
const DMX_HEADER_LEN: usize = 18;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Packet<'a> {
    /// OpDmx. `universe` is the 15-bit port address (Net:SubNet:Universe).
    Dmx {
        universe: u16,
        sequence: u8,
        data: &'a [u8],
    },
    Poll,
    Sync,
    /// OpAddress: remote programming of name and port-addresses.
    Address(Address),
    Other(u16),
}

/// ArtAddress commands for merging (Art-Net 4): cancel, LTP / HTP per port.
pub const AC_CANCEL_MERGE: u8 = 0x01;
pub const AC_MERGE_LTP: core::ops::RangeInclusive<u8> = 0x10..=0x13;
pub const AC_MERGE_HTP: core::ops::RangeInclusive<u8> = 0x50..=0x53;

/// The changes an ArtAddress asks for (`None` = no change).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Address {
    /// The reply group ("bind", from 1) the port fields apply to.
    pub bind_index: u8,
    pub net: Option<u8>,
    pub sub: Option<u8>,
    pub sw_out: [Option<u8>; 4],
    /// New short name (NUL-terminated in the packet; empty = no change).
    pub short_name: [u8; 18],
    pub command: u8,
}

impl Address {
    pub fn short_name(&self) -> Option<&str> {
        let n = self.short_name.iter().position(|&b| b == 0).unwrap_or(self.short_name.len());
        core::str::from_utf8(&self.short_name[..n]).ok().filter(|s| !s.is_empty())
    }

    /// The port-address `old` becomes (same if nothing changes for port `i`).
    pub fn apply(&self, i: usize, old: u16) -> u16 {
        let net = self.net.map_or(old >> 8 & 0x7f, u16::from);
        let sub = self.sub.map_or(old >> 4 & 0xf, u16::from);
        let port = self.sw_out.get(i).copied().flatten().map_or(old & 0xf, u16::from);
        (net << 8) | (sub << 4) | port
    }
}

/// A switch byte of ArtAddress: bit 7 set = program the low bits; anything
/// else (0x7f "no change", 0x00 "reset to the physical switches", which this
/// node has none of) changes nothing.
fn program(b: u8, mask: u8) -> Option<u8> {
    (b & 0x80 != 0).then_some(b & mask)
}

/// Parses one UDP payload. Returns `None` for anything that is not a valid
/// Art-Net packet.
#[cfg_attr(target_arch = "xtensa", esp_hal::ram)]
pub fn parse(p: &[u8]) -> Option<Packet<'_>> {
    if p.len() < 10 || &p[..8] != ID {
        return None;
    }
    let opcode = u16::from_le_bytes([p[8], p[9]]);
    match opcode {
        OP_DMX => {
            if p.len() < DMX_HEADER_LEN {
                return None;
            }
            let sequence = p[12];
            let universe = (u16::from(p[15] & 0x7f) << 8) | u16::from(p[14]);
            let len = usize::from(u16::from_be_bytes([p[16], p[17]]));
            if !(2..=DMX_CHANNELS).contains(&len) || p.len() < DMX_HEADER_LEN + len {
                return None;
            }
            Some(Packet::Dmx {
                universe,
                sequence,
                data: &p[DMX_HEADER_LEN..DMX_HEADER_LEN + len],
            })
        }
        OP_POLL => Some(Packet::Poll),
        OP_ADDRESS => {
            if p.len() < 107 {
                return None;
            }
            let mut short_name = [0u8; 18];
            short_name.copy_from_slice(&p[14..32]);
            Some(Packet::Address(Address {
                bind_index: p[13].max(1),
                net: program(p[12], 0x7f),
                sub: program(p[104], 0x0f),
                sw_out: core::array::from_fn(|i| program(p[100 + i], 0x0f)),
                short_name,
                command: p[106],
            }))
        }
        OP_SYNC => Some(Packet::Sync),
        other => Some(Packet::Other(other)),
    }
}

/// Length of an ArtDmx packet with 512 channels.
pub const DMX_PACKET_LEN: usize = DMX_HEADER_LEN + DMX_CHANNELS;

/// Writes an ArtDmx packet for port-address `universe` (sequence 1..=255;
/// 0 turns sequencing off) into `p`: 512 channels, `channels` first and
/// the rest 0. Returns its length ([`DMX_PACKET_LEN`]); `p` must hold it.
pub fn write_dmx(p: &mut [u8], universe: u16, sequence: u8, channels: &[u8]) -> usize {
    let p = &mut p[..DMX_PACKET_LEN];
    p.fill(0);
    p[..8].copy_from_slice(ID);
    p[8..10].copy_from_slice(&OP_DMX.to_le_bytes());
    p[10..12].copy_from_slice(&14u16.to_be_bytes());
    p[12] = sequence;
    p[14] = (universe & 0xff) as u8;
    p[15] = ((universe >> 8) & 0x7f) as u8;
    p[16..18].copy_from_slice(&(DMX_CHANNELS as u16).to_be_bytes());
    let n = channels.len().min(DMX_CHANNELS);
    p[DMX_HEADER_LEN..DMX_HEADER_LEN + n].copy_from_slice(&channels[..n]);
    DMX_PACKET_LEN
}

/// Universes 0..TRACKED get per-universe counters; higher ones are only
/// counted in total.
pub const TRACKED: usize = 64;

/// Counters for one reporting window.
pub struct Stats {
    packets: u32,
    invalid: u32,
    dmx: u32,
    polls: u32,
    syncs: u32,
    untracked: u32,
    seq_gaps: u32,
    per_universe: [u32; TRACKED],
    last_seq: [u8; TRACKED],
}

impl Default for Stats {
    fn default() -> Self {
        Self::new()
    }
}

impl Stats {
    pub const fn new() -> Self {
        Self {
            packets: 0,
            invalid: 0,
            dmx: 0,
            polls: 0,
            syncs: 0,
            untracked: 0,
            seq_gaps: 0,
            per_universe: [0; TRACKED],
            last_seq: [0; TRACKED],
        }
    }

    #[cfg_attr(target_arch = "xtensa", esp_hal::ram)]
    pub fn record(&mut self, packet: Option<&Packet<'_>>) {
        self.packets += 1;
        match packet {
            None => self.invalid += 1,
            Some(Packet::Poll) => self.polls += 1,
            Some(Packet::Sync) => self.syncs += 1,
            Some(Packet::Other(_)) | Some(Packet::Address(_)) => {}
            Some(Packet::Dmx {
                universe, sequence, ..
            }) => {
                self.dmx += 1;
                let u = usize::from(*universe);
                if u >= TRACKED {
                    self.untracked += 1;
                    return;
                }
                self.per_universe[u] += 1;
                // Sequence 0 means an Art-Net sender does not number its
                // packets; otherwise Art-Net counts 1..=255 and wraps to 1,
                // sACN counts 0..=255 and wraps to 0.
                let last = self.last_seq[u];
                if *sequence != 0 && last != 0 {
                    if *sequence != last.wrapping_add(1) && !(last == 255 && *sequence == 1) {
                        self.seq_gaps += 1;
                    }
                } else if *sequence == 0 && last != 0 && last != 255 {
                    // 0 after something other than 255: a gap (sACN) or a
                    // sender that stopped numbering (Art-Net): count once.
                    self.seq_gaps += 1;
                }
                self.last_seq[u] = *sequence;
            }
        }
    }

    /// Summarises the window that lasted `elapsed_ms` and starts a new one.
    /// Sequence numbers carry over so a gap across windows is still caught.
    pub fn take_report(&mut self, elapsed_ms: u64) -> Report {
        let ms = elapsed_ms.max(1);
        let rate = |n: u32| (u64::from(n) * 1000 / ms) as u32;

        let mut universes = 0;
        let mut fps_min = u32::MAX;
        let mut fps_max = 0;
        for &n in self.per_universe.iter().filter(|&&n| n > 0) {
            universes += 1;
            fps_min = fps_min.min(rate(n));
            fps_max = fps_max.max(rate(n));
        }
        if universes == 0 {
            fps_min = 0;
        }

        let mut per_universe = [0u16; TRACKED];
        for (fps, &n) in per_universe.iter_mut().zip(self.per_universe.iter()) {
            *fps = rate(n).min(u32::from(u16::MAX)) as u16;
        }
        let report = Report {
            per_universe,
            pps: rate(self.packets),
            dmx: self.dmx,
            invalid: self.invalid,
            polls: self.polls,
            syncs: self.syncs,
            untracked: self.untracked,
            seq_gaps: self.seq_gaps,
            universes,
            fps_min,
            fps_max,
        };

        let last_seq = self.last_seq;
        *self = Self::new();
        self.last_seq = last_seq;
        report
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Report {
    /// Frames per second of universes 0..TRACKED (0 = none received).
    pub per_universe: [u16; TRACKED],
    pub pps: u32,
    pub dmx: u32,
    pub invalid: u32,
    pub polls: u32,
    pub syncs: u32,
    pub untracked: u32,
    pub seq_gaps: u32,
    pub universes: u32,
    pub fps_min: u32,
    pub fps_max: u32,
}

impl fmt::Display for Report {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "pps={} dmx={} universes={} fps={}..{} seq_gaps={} sync={} poll={} invalid={} untracked={}",
            self.pps,
            self.dmx,
            self.universes,
            self.fps_min,
            self.fps_max,
            self.seq_gaps,
            self.syncs,
            self.polls,
            self.invalid,
            self.untracked,
        )
    }
}

// ── Discovery: ArtPollReply ────────────────────────────────────────────────

/// Length of an ArtPollReply (Art-Net 4).
pub const POLL_REPLY_LEN: usize = 239;
/// Ports per ArtPollReply; a node with more sends one reply per group
/// ("bind"), numbered by BindIndex.
const PORTS_PER_REPLY: usize = 4;

/// What a reply says about the node.
pub struct NodeInfo<'a> {
    pub ip: [u8; 4],
    pub mac: [u8; 6],
    pub short_name: &'a str,
    pub long_name: &'a str,
    /// Firmware version (high and low byte).
    pub version: u16,
    /// Address came from DHCP.
    pub dhcp: bool,
    /// Highest frame rate the outputs can show (0 = unknown).
    pub refresh_hz: u16,
    /// ArtPolls answered so far (for the node report).
    pub polls: u16,
    /// DMX data arrived recently.
    pub active: bool,
    /// Merge mode is LTP (else HTP).
    pub ltp: bool,
    /// Port-addresses on which two sources are being merged.
    pub merging: &'a [u16],
}

/// One group of up to four output universes sharing Net and SubNet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bind {
    /// Port-address bits 14..4 (Net and SubNet).
    pub net_sub: u16,
    /// Low 4 bits (universe within the SubNet) of each port.
    pub ports: heapless_ports::Ports,
}

pub mod heapless_ports {
    /// Up to four port numbers (0..16) without allocation.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
    pub struct Ports {
        pub n: u8,
        pub p: [u8; 4],
    }
    impl Ports {
        pub fn as_slice(&self) -> &[u8] {
            &self.p[..usize::from(self.n)]
        }
    }
}

/// Groups `universes` (any order, duplicates allowed) into binds of at most
/// four ports that share Net and SubNet, in ascending order. Calls `f` for
/// each, so no allocation is needed.
pub fn for_each_bind(universes: &mut [u16], mut f: impl FnMut(&Bind)) {
    universes.sort_unstable();
    let mut i = 0;
    while i < universes.len() {
        let net_sub = universes[i] >> 4;
        let mut b = Bind { net_sub, ports: Default::default() };
        while i < universes.len() && universes[i] >> 4 == net_sub && usize::from(b.ports.n) < PORTS_PER_REPLY {
            let port = (universes[i] & 0xf) as u8;
            if !b.ports.as_slice().contains(&port) {
                b.ports.p[usize::from(b.ports.n)] = port;
                b.ports.n += 1;
            }
            i += 1;
            // Skip duplicates of the port just added.
            while i < universes.len() && universes[i] == universes[i - 1] {
                i += 1;
            }
        }
        f(&b);
    }
}

fn put_str(dst: &mut [u8], s: &str) {
    // NUL-terminated, cut to fit.
    let n = s.len().min(dst.len() - 1);
    dst[..n].copy_from_slice(&s.as_bytes()[..n]);
}

/// Builds one ArtPollReply for `bind` (`bind_index` starts at 1).
pub fn poll_reply(out: &mut [u8; POLL_REPLY_LEN], node: &NodeInfo<'_>, bind: &Bind, bind_index: u8) {
    out.fill(0);
    out[..8].copy_from_slice(ID);
    out[8..10].copy_from_slice(&OP_POLL_REPLY.to_le_bytes());
    out[10..14].copy_from_slice(&node.ip);
    out[14..16].copy_from_slice(&PORT.to_le_bytes());
    out[16..18].copy_from_slice(&node.version.to_be_bytes());
    out[18] = ((bind.net_sub >> 4) & 0x7f) as u8; // NetSwitch
    out[19] = (bind.net_sub & 0xf) as u8; // SubSwitch
    out[20] = 0x00; // OEM: unknown (0x00FF)
    out[21] = 0xff;
    // Status1: indicators normal; port-address set from the network/web.
    out[23] = 0b1110_0000;
    put_str(&mut out[26..44], node.short_name);
    put_str(&mut out[44..108], node.long_name);
    let report = alloc::format!("#0001 [{:04}] OK", node.polls % 10000);
    put_str(&mut out[108..172], &report);
    let ports = bind.ports.as_slice();
    out[173] = ports.len() as u8; // NumPortsLo
    for (i, &p) in ports.iter().enumerate() {
        out[174 + i] = 0x80; // PortTypes: output, DMX512 protocol
        // GoodOutputA: bit 7 data being sent, bit 3 merging, bit 1 LTP.
        let pa = (bind.net_sub << 4) | u16::from(p);
        out[182 + i] = if node.active { 0x80 } else { 0x00 }
            | if node.merging.contains(&pa) { 0x08 } else { 0x00 }
            | if node.ltp { 0x02 } else { 0x00 };
        out[190 + i] = p; // SwOut
        out[213 + i] = 0xc0; // GoodOutputB: RDM off, continuous output
    }
    out[200] = 0x00; // Style: StNode
    out[201..207].copy_from_slice(&node.mac);
    out[207..211].copy_from_slice(&node.ip); // BindIp
    out[211] = bind_index;
    // Status2: bit 0 web configuration, bit 1 IP from DHCP, bit 2 DHCP
    // capable, bit 3 15-bit port-addresses.
    out[212] = 0x08 | 0x04 | 0x01 | if node.dhcp { 0x02 } else { 0 };
    out[226..228].copy_from_slice(&node.refresh_hz.to_be_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dmx_packet_parses() {
        let mut p = [0xAAu8; DMX_PACKET_LEN + 4];
        assert_eq!(write_dmx(&mut p, 0x1234, 42, &[7, 8]), DMX_PACKET_LEN);
        let mut data = [0u8; DMX_CHANNELS];
        data[..2].copy_from_slice(&[7, 8]);
        assert_eq!(parse(&p), Some(Packet::Dmx { universe: 0x1234, sequence: 42, data: &data[..] }));
    }

    /// An Ethernet/IPv4/UDP frame to port `port` with `payload`.
    fn udp_frame(dst: [u8; 6], port: u16, payload: &[u8]) -> alloc::vec::Vec<u8> {
        let mut f = alloc::vec::Vec::new();
        f.extend_from_slice(&dst);
        f.extend_from_slice(&[2, 0, 0, 0, 0, 1, 0x08, 0x00]);
        let ip_len = (20 + 8 + payload.len()) as u16;
        f.extend_from_slice(&[0x45, 0, (ip_len >> 8) as u8, ip_len as u8, 0, 0, 0x40, 0, 64, 17, 0, 0]);
        f.extend_from_slice(&[2, 0, 0, 7, 2, 0, 0, 10]);
        let udp_len = (8 + payload.len()) as u16;
        f.extend_from_slice(&[0x19, 0x36, (port >> 8) as u8, port as u8, (udp_len >> 8) as u8, udp_len as u8, 0, 0]);
        f.extend_from_slice(payload);
        f
    }

    #[test]
    fn udp_payload_plain_and_rejected() {
        let mac = [2, 0, 0, 0, 0, 2];
        let f = udp_frame(mac, PORT, b"Art-Net\0");
        assert_eq!(udp_payload(&f, &mac), Some(([2, 0, 0, 7], &b"Art-Net\0"[..])));
        let mut padded = f.clone();
        padded.extend_from_slice(&[0; 10]); // Ethernet padding is ignored
        assert_eq!(udp_payload(&padded, &mac).unwrap().1.len(), 8);
        assert!(udp_payload(&udp_frame([0xff; 6], PORT, b"x"), &mac).is_some(), "broadcast");
        assert!(udp_payload(&udp_frame([2, 0, 0, 0, 0, 9], PORT, b"x"), &mac).is_none(), "other MAC");
        assert!(udp_payload(&udp_frame(mac, 5568, b"x"), &mac).is_none(), "other port");
        let mut frag = f.clone();
        frag[14 + 6] |= 0x20; // more fragments
        assert!(udp_payload(&frag, &mac).is_none());
        let mut opts = f.clone();
        opts[14] = 0x46; // IP options
        assert!(udp_payload(&opts, &mac).is_none());
        let mut tcp = f.clone();
        tcp[14 + 9] = 6;
        assert!(udp_payload(&tcp, &mac).is_none());
        assert!(udp_payload(&f[..f.len() - 1], &mac).is_none(), "cut short");
        for n in 0..42 {
            assert!(udp_payload(&f[..n], &mac).is_none());
        }
    }
    extern crate std;
    use std::vec::Vec;

    #[test]
    fn address_parses_and_applies() {
        let mut p = std::vec![0u8; 107];
        p[..8].copy_from_slice(ID);
        p[8..10].copy_from_slice(&OP_ADDRESS.to_le_bytes());
        p[12] = 0x7f; // net: no change
        p[13] = 2; // bind 2
        p[14..19].copy_from_slice(b"Stage");
        p[100] = 0x85; // port 0 -> 5
        p[101] = 0x7f;
        p[104] = 0x83; // subnet -> 3
        let Some(Packet::Address(a)) = parse(&p) else { panic!("not parsed") };
        assert_eq!(a.bind_index, 2);
        assert_eq!(a.short_name(), Some("Stage"));
        assert_eq!((a.net, a.sub, a.sw_out[0], a.sw_out[1]), (None, Some(3), Some(5), None));
        // 0x0104 (net 1, sub 0, port 4): sub -> 3, port 0 -> 5.
        assert_eq!(a.apply(0, 0x0104), 0x0135);
        assert_eq!(a.apply(1, 0x0104), 0x0134);
        let mut q = p.clone();
        q[14] = 0;
        let Some(Packet::Address(b)) = parse(&q) else { panic!() };
        assert_eq!(b.short_name(), None);
    }

    #[test]
    fn binds_group_by_net_sub_and_four() {
        let mut u = [0u16, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 3, 0x123];
        let mut got = Vec::new();
        for_each_bind(&mut u, |b| got.push((b.net_sub, b.ports.as_slice().to_vec())));
        assert_eq!(
            got,
            [
                (0, std::vec![0, 1, 2, 3]),
                (0, std::vec![4, 5, 6, 7]),
                (0, std::vec![8, 9, 10, 11]),
                (0, std::vec![12, 13, 14, 15]),
                (1, std::vec![0]),
                (0x12, std::vec![3]),
            ]
        );
    }

    #[test]
    fn sequence_gaps_wrap_both_ways() {
        let feed = |seqs: &[u8]| {
            let mut st = Stats::new();
            for &q in seqs {
                st.record(Some(&Packet::Dmx { universe: 3, sequence: q, data: &[] }));
            }
            st.seq_gaps
        };
        assert_eq!(feed(&[253, 254, 255, 1, 2]), 0); // Art-Net wraps to 1
        assert_eq!(feed(&[253, 254, 255, 0, 1, 2]), 0); // sACN wraps to 0
        assert_eq!(feed(&[0, 0, 0]), 0); // Art-Net without numbering
        assert_eq!(feed(&[5, 7]), 1);
        assert_eq!(feed(&[5, 0]), 1);
    }

    #[test]
    fn poll_reply_layout() {
        let node = NodeInfo {
            ip: [2, 14, 66, 63],
            mac: [1, 2, 3, 4, 5, 6],
            short_name: "Node",
            long_name: "Node long name",
            version: 0x0102,
            dhcp: false,
            refresh_hz: 72,
            polls: 3,
            active: true,
            ltp: true,
            merging: &[0x124],
        };
        let mut b = Bind { net_sub: 0x12, ports: Default::default() };
        b.ports.p = [3, 4, 0, 0];
        b.ports.n = 2;
        let mut out = [0u8; POLL_REPLY_LEN];
        poll_reply(&mut out, &node, &b, 2);
        assert_eq!(&out[..8], b"Art-Net\0");
        assert_eq!(&out[8..10], &[0x00, 0x21]);
        assert_eq!(&out[10..16], &[2, 14, 66, 63, 0x36, 0x19]);
        assert_eq!((out[18], out[19]), (0x01, 0x02));
        assert_eq!(&out[26..31], b"Node\0");
        assert_eq!(out[173], 2);
        assert_eq!(&out[174..176], &[0x80, 0x80]);
        assert_eq!(&out[190..194], &[3, 4, 0, 0]);
        // Port 4 (0x124) is merging; LTP flagged on both ports.
        assert_eq!(&out[182..184], &[0x82, 0x8a]);
        assert_eq!(&out[201..207], &[1, 2, 3, 4, 5, 6]);
        assert_eq!(out[211], 2);
        assert_eq!(out[212], 0x0d, "static IP: web config, DHCP capable, 15-bit");
        assert_eq!(&out[226..228], &[0, 72]);
        assert!(std::str::from_utf8(&out[108..124]).unwrap().starts_with("#0001 [0003] OK"));
    }
}
