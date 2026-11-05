//! A minimal mDNS responder: answers A queries for `<host>.local` so the
//! web interface opens as e.g. `http://artnetnode-423c.local/`.

pub const PORT: u16 = 5353;
pub const GROUP: [u8; 4] = [224, 0, 0, 251];
const TYPE_A: u16 = 1;
const TYPE_ANY: u16 = 255;
const CLASS_IN: u16 = 1;
/// Seconds a resolver may cache the answer.
const TTL: u32 = 120;

/// The host label for a device name: lowercase letters, digits and
/// hyphens, at most 63 characters ("ArtnetNode 423C" → "artnetnode-423c").
pub fn host_label(name: &str) -> heapless_label::Label {
    let mut l = heapless_label::Label::default();
    let mut dash = false;
    for c in name.chars() {
        let c = c.to_ascii_lowercase();
        if c.is_ascii_alphanumeric() {
            l.push(c as u8);
            dash = false;
        } else if !dash && l.len() > 0 {
            l.push(b'-');
            dash = true;
        }
    }
    while l.as_bytes().last() == Some(&b'-') {
        l.pop();
    }
    if l.len() == 0 {
        for &b in b"artnetnode" {
            l.push(b);
        }
    }
    l
}

pub mod heapless_label {
    /// Up to 63 bytes of DNS label.
    #[derive(Clone, Copy)]
    pub struct Label {
        b: [u8; 63],
        n: usize,
    }
    impl Default for Label {
        fn default() -> Self {
            Self { b: [0; 63], n: 0 }
        }
    }
    impl Label {
        pub fn push(&mut self, c: u8) {
            if self.n < self.b.len() {
                self.b[self.n] = c;
                self.n += 1;
            }
        }
        pub fn pop(&mut self) {
            self.n = self.n.saturating_sub(1);
        }
        pub fn len(&self) -> usize {
            self.n
        }
        pub fn as_bytes(&self) -> &[u8] {
            &self.b[..self.n]
        }
    }
}

fn be16(p: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_be_bytes([*p.get(at)?, *p.get(at + 1)?]))
}

/// Whether the name at `at` is `<host>.local` (case-insensitive; no
/// compression pointers in questions). Returns the offset after the name.
fn name_is(p: &[u8], mut at: usize, host: &[u8]) -> Option<(bool, usize)> {
    let mut labels = 0;
    let mut matches = true;
    loop {
        let len = usize::from(*p.get(at)?);
        at += 1;
        if len == 0 {
            return Some((matches && labels == 2, at));
        }
        if len & 0xC0 != 0 {
            return None; // compression in a question: not ours to handle
        }
        let label = p.get(at..at + len)?;
        let want: &[u8] = match labels {
            0 => host,
            1 => b"local",
            _ => b"",
        };
        if !label.eq_ignore_ascii_case(want) {
            matches = false;
        }
        labels += 1;
        at += len;
    }
}

/// If `query` asks for the A record of `<host>.local`, writes the answer
/// into `out` and returns its length.
pub fn answer(query: &[u8], host: &[u8], ip: [u8; 4], out: &mut [u8]) -> Option<usize> {
    let flags = be16(query, 2)?;
    if flags & 0x8000 != 0 {
        return None; // a response, not a query
    }
    let questions = be16(query, 4)?;
    let mut at = 12;
    for _ in 0..questions {
        let (ours, next) = name_is(query, at, host)?;
        let qtype = be16(query, next)?;
        let qclass = be16(query, next + 2)? & 0x7FFF;
        at = next + 4;
        if ours && (qtype == TYPE_A || qtype == TYPE_ANY) && qclass == CLASS_IN {
            return Some(record(host, ip, out));
        }
    }
    None
}

/// An unsolicited announcement / answer: one A record for `<host>.local`.
pub fn record(host: &[u8], ip: [u8; 4], out: &mut [u8]) -> usize {
    let mut n = 0;
    let mut put = |b: &[u8]| {
        out[n..n + b.len()].copy_from_slice(b);
        n += b.len();
    };
    // Header: id 0, response + authoritative, 0 questions, 1 answer.
    put(&[0, 0, 0x84, 0, 0, 0, 0, 1, 0, 0, 0, 0]);
    put(&[host.len() as u8]);
    put(host);
    put(&[5]);
    put(b"local");
    put(&[0]);
    put(&TYPE_A.to_be_bytes());
    // Class IN with the cache-flush bit: this is the only owner of the name.
    put(&(CLASS_IN | 0x8000).to_be_bytes());
    put(&TTL.to_be_bytes());
    put(&4u16.to_be_bytes());
    put(&ip);
    n
}

#[cfg(test)]
mod tests {
    use super::*;
    extern crate std;
    use std::vec::Vec;

    fn query(name: &[&str], qtype: u16) -> Vec<u8> {
        let mut q = std::vec![0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0];
        for l in name {
            q.push(l.len() as u8);
            q.extend_from_slice(l.as_bytes());
        }
        q.push(0);
        q.extend_from_slice(&qtype.to_be_bytes());
        q.extend_from_slice(&0x8001u16.to_be_bytes()); // QU bit set
        q
    }

    #[test]
    fn host_label_from_name() {
        assert_eq!(host_label("ArtnetNode-423C").as_bytes(), b"artnetnode-423c");
        assert_eq!(host_label("  Stage left / #2 ").as_bytes(), b"stage-left-2");
        assert_eq!(host_label("!!!").as_bytes(), b"artnetnode");
    }

    #[test]
    fn answers_only_our_a_query() {
        let host = b"artnetnode-423c";
        let mut out = [0u8; 128];
        let n = answer(&query(&["ArtnetNode-423C", "local"], TYPE_A), host, [2, 14, 66, 63], &mut out).unwrap();
        assert_eq!(&out[n - 4..n], &[2, 14, 66, 63]);
        assert_eq!(&out[2..4], &[0x84, 0]);
        assert!(answer(&query(&["other", "local"], TYPE_A), host, [1, 2, 3, 4], &mut out).is_none());
        assert!(answer(&query(&["artnetnode-423c", "local"], 28), host, [1, 2, 3, 4], &mut out).is_none());
        assert!(answer(&[0, 0, 0x84, 0], host, [1, 2, 3, 4], &mut out).is_none());
    }
}
