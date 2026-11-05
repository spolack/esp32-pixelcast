//! Dual boot: what this firmware hands over to the firmware in the other
//! app slot (WLED, or another one; see dualboot/README.md).
//!
//! Sector 2 of the `storage` partition is the handoff, written each time this
//! firmware starts the other one:
//! - at 0, the address record (this module, the same for every firmware);
//! - at [`SETUP_OFFSET`], the LED setup in the other firmware's own format,
//!   from its adapter ([`wled`]).
//!
//! Sector 3 is the other firmware's log, if it writes one: 4-byte tag, u32
//! length (little endian), text. This firmware shows it as
//! `GET /api/dualboot-log`.
//!
//! Address record (little endian), 24 bytes:
//!   0 magic "PXCW", 4 version 1, 5 mode (0 = DHCP: the other firmware keeps
//!   its own settings; 1 = static), 6 prefix, 7..8 zero, 8 ip, 12 gateway
//!   (0.0.0.0: none), 16..20 zero, 20 CRC-32 of bytes 0..20.

use core::net::Ipv4Addr;

pub mod wled;

/// Sector of `storage` with the handoff, and the one with the log.
pub const HANDOFF_SECTOR: u32 = 2;
pub const LOG_SECTOR: u32 = 3;
pub const ADDRESS_LEN: usize = 24;
/// Where in the handoff sector the LED setup starts, and its room.
pub const SETUP_OFFSET: u32 = 0x100;
pub const SETUP_MAX: usize = 0x100;

/// The address record for `addr` (the address this firmware has now, if
/// any): static unless it came from DHCP (then the other firmware's DHCP
/// gets the same lease for the same MAC).
pub fn address_record(addr: Option<(Ipv4Addr, u8)>, from_dhcp: bool, gateway: Option<Ipv4Addr>) -> [u8; ADDRESS_LEN] {
    let mut r = [0u8; ADDRESS_LEN];
    r[..4].copy_from_slice(b"PXCW");
    r[4] = 1;
    if let (Some((ip, prefix)), false) = (addr, from_dhcp) {
        r[5] = 1;
        r[6] = prefix;
        r[8..12].copy_from_slice(&ip.octets());
        r[12..16].copy_from_slice(&gateway.map_or([0; 4], |g| g.octets()));
    }
    let crc = crate::inplace::crc32(&r[..20]);
    r[20..24].copy_from_slice(&crc.to_le_bytes());
    r
}

/// The text of a log sector, if it holds one.
pub fn log_text(sector: &[u8]) -> Option<&[u8]> {
    let len = u32::from_le_bytes(sector.get(4..8)?.try_into().ok()?) as usize;
    (len != 0 && len <= sector.len() - 8).then(|| &sector[8..8 + len])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn address_record_static_and_dhcp() {
        let r = address_record(Some((Ipv4Addr::new(2, 1, 2, 3), 8)), false, Some(Ipv4Addr::new(2, 0, 0, 1)));
        assert_eq!(&r[..8], b"PXCW\x01\x01\x08\x00");
        assert_eq!(&r[8..16], &[2, 1, 2, 3, 2, 0, 0, 1]);
        assert_eq!(u32::from_le_bytes([r[20], r[21], r[22], r[23]]), crate::inplace::crc32(&r[..20]));
        let d = address_record(Some((Ipv4Addr::new(10, 0, 0, 5), 24)), true, None);
        assert_eq!(d[5], 0);
        assert_eq!(&d[8..16], &[0; 8]);
    }

    #[test]
    fn log() {
        let mut s = [0xFFu8; 64];
        s[..8].copy_from_slice(b"WLOG\x03\x00\x00\x00");
        s[8..11].copy_from_slice(b"abc");
        assert_eq!(log_text(&s), Some(&b"abc"[..]));
        assert_eq!(log_text(&[0xFF; 64]), None);
        assert_eq!(log_text(&[0; 64]), None);
    }
}
