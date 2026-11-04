//! A DHCP server probe: one DISCOVER, and recognising an OFFER for it.
//!
//! On the Art-Net fallback address the board looks for a DHCP server now and
//! then. Running the real DHCP client for that means dropping the address
//! for the whole attempt, which closes every connection (an upload, the
//! settings page). The probe keeps the address and only says whether a
//! server answers; the real client then takes over.

/// Server and client ports.
pub const SERVER_PORT: u16 = 67;
pub const CLIENT_PORT: u16 = 68;

/// Length of [`discover`]'s packet: the fixed BOOTP part (236), the magic
/// cookie and options, padded to the 300 bytes some servers expect.
pub const DISCOVER_LEN: usize = 300;

const MAGIC: [u8; 4] = [99, 130, 83, 99];

/// A DHCPDISCOVER from `mac` with transaction id `xid`, asking for the
/// answer as a broadcast (the board has no address of the server's
/// network to receive a unicast on).
pub fn discover(xid: u32, mac: [u8; 6]) -> [u8; DISCOVER_LEN] {
    let mut p = [0u8; DISCOVER_LEN];
    p[0] = 1; // BOOTREQUEST
    p[1] = 1; // Ethernet
    p[2] = 6; // address length
    p[4..8].copy_from_slice(&xid.to_be_bytes());
    p[10] = 0x80; // flags: broadcast
    p[28..34].copy_from_slice(&mac);
    p[236..240].copy_from_slice(&MAGIC);
    // 53 message type = DISCOVER; 55 parameter list: mask, router, DNS; end.
    p[240..250].copy_from_slice(&[53, 1, 1, 55, 3, 1, 3, 6, 255, 0]);
    p
}

/// Whether `p` is a DHCPOFFER answering the DISCOVER `xid` from `mac`.
pub fn is_offer(p: &[u8], xid: u32, mac: [u8; 6]) -> bool {
    if p.len() < 240 || p[0] != 2 || p[4..8] != xid.to_be_bytes() || p[28..34] != mac || p[236..240] != MAGIC {
        return false;
    }
    // Walk the options for 53 (message type).
    let mut i = 240;
    while i < p.len() {
        match p[i] {
            0 => i += 1,
            255 => return false,
            code => {
                let Some(&len) = p.get(i + 1) else { return false };
                let len = len as usize;
                if code == 53 && len == 1 {
                    return p.get(i + 2) == Some(&2);
                }
                i += 2 + len;
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAC: [u8; 6] = [0x28, 0x05, 0xa5, 0x0e, 0x42, 0x3f];

    fn offer(xid: u32, msg_type: u8) -> [u8; 300] {
        let mut p = discover(xid, MAC);
        p[0] = 2;
        p[16..20].copy_from_slice(&[192, 168, 1, 50]); // yiaddr
        // A pad, the server id, then the message type.
        p[240..254].copy_from_slice(&[0, 54, 4, 192, 168, 1, 1, 53, 1, msg_type, 255, 0, 0, 0]);
        p
    }

    #[test]
    fn discover_layout() {
        let p = discover(0x1234_5678, MAC);
        assert_eq!(&p[..4], &[1, 1, 6, 0]);
        assert_eq!(&p[4..8], &[0x12, 0x34, 0x56, 0x78]);
        assert_eq!(p[10], 0x80);
        assert_eq!(&p[28..34], &MAC);
        assert_eq!(&p[236..243], &[99, 130, 83, 99, 53, 1, 1]);
    }

    #[test]
    fn offer_recognised() {
        assert!(is_offer(&offer(7, 2), 7, MAC));
    }

    #[test]
    fn others_rejected() {
        assert!(!is_offer(&offer(7, 2), 8, MAC), "other transaction");
        assert!(!is_offer(&offer(7, 5), 7, MAC), "an ACK, not an offer");
        assert!(!is_offer(&offer(7, 2), 7, [0; 6]), "other client");
        assert!(!is_offer(&discover(7, MAC), 7, MAC), "our own request");
        assert!(!is_offer(&offer(7, 2)[..239], 7, MAC), "too short");
        let mut cut = offer(7, 2);
        cut[242] = 250; // option length past the end
        assert!(!is_offer(&cut, 7, MAC));
    }
}
