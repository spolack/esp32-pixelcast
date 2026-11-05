//! In-place update: the record that hands the board over to the updater.
//!
//! The updater (`updater/`) is a small firmware that runs from RAM only. To
//! replace the running firmware without touching the other slot (e.g.
//! WLED), the firmware writes the updater and this record into the unused
//! end of its own slot and restarts. The bootloader (`bootloader/`) finds the
//! record, starts the updater, and after the upload checks the new image and
//! selects it. Layout inside the slot (offsets from its start):
//!
//! | offset     | size   | content                          |
//! |------------|--------|----------------------------------|
//! | 0          | ≤ 1.46 MB | the firmware image             |
//! | `UPDATER`  | 32 KB  | the updater image                |
//! | `RECORD`   | 4 KB   | this record (48 bytes), a note at `NOTE` |
//!
//! Record (little endian): 0 magic "PXCU", 4 version 1, 5 state, 6 updater
//! starts so far, 7 prefix length, 8 IPv4 address, 12 gateway (0.0.0.0:
//! none), 16 MAC, 22..28 zero, 28 upload token (16 bytes: the updater has
//! no login, so only the page that asked for the update may upload), 44
//! CRC-32 of bytes 0..44.
//!
//! When the updater gives up it erases the record and leaves a short text
//! at `RECORD + NOTE` (NUL-terminated) saying why.
//!
//! The same layout is read and written by the bootloader hook (C); keep both
//! in step.

/// Offset of the updater image in the slot.
pub const UPDATER: u32 = 0x17_7000;
/// Largest updater image.
pub const UPDATER_MAX: u32 = 0x8000;
/// Offset of the record sector in the slot.
pub const RECORD: u32 = 0x17_F000;
/// Largest firmware image an in-place update can take.
pub const IMAGE_MAX: u32 = UPDATER;

pub const LEN: usize = 48;
/// Offset of the updater's note in the record sector.
pub const NOTE: u32 = 0x800;
pub const NOTE_MAX: usize = 128;
const MAGIC: [u8; 4] = *b"PXCU";
const VERSION: u8 = 1;

/// Where the update stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    /// Requested; the slot still holds the old firmware.
    Start = 1,
    /// The updater began erasing the slot: the old firmware is gone.
    Writing = 2,
    /// The new image is written; the bootloader checks and selects it.
    Written = 3,
}

impl State {
    fn from_u8(v: u8) -> Option<State> {
        match v {
            1 => Some(State::Start),
            2 => Some(State::Writing),
            3 => Some(State::Written),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Record {
    pub state: State,
    /// How often the bootloader started the updater in `Start` state.
    pub attempts: u8,
    pub ip: [u8; 4],
    pub prefix: u8,
    pub gateway: [u8; 4],
    pub mac: [u8; 6],
    pub token: [u8; 16],
}

impl Record {
    pub fn encode(&self) -> [u8; LEN] {
        let mut r = [0u8; LEN];
        r[..4].copy_from_slice(&MAGIC);
        r[4] = VERSION;
        r[5] = self.state as u8;
        r[6] = self.attempts;
        r[7] = self.prefix;
        r[8..12].copy_from_slice(&self.ip);
        r[12..16].copy_from_slice(&self.gateway);
        r[16..22].copy_from_slice(&self.mac);
        r[28..44].copy_from_slice(&self.token);
        let crc = crc32(&r[..44]);
        r[44..48].copy_from_slice(&crc.to_le_bytes());
        r
    }

    /// None for an erased sector, a damaged record or another version.
    pub fn decode(r: &[u8]) -> Option<Record> {
        if r.len() < LEN || r[..4] != MAGIC || r[4] != VERSION {
            return None;
        }
        if u32::from_le_bytes([r[44], r[45], r[46], r[47]]) != crc32(&r[..44]) {
            return None;
        }
        if r[7] > 32 {
            return None;
        }
        Some(Record {
            state: State::from_u8(r[5])?,
            attempts: r[6],
            prefix: r[7],
            ip: [r[8], r[9], r[10], r[11]],
            gateway: [r[12], r[13], r[14], r[15]],
            mac: [r[16], r[17], r[18], r[19], r[20], r[21]],
            token: {
                let mut t = [0u8; 16];
                t.copy_from_slice(&r[28..44]);
                t
            },
        })
    }
}

/// CRC-32 (IEEE, as zlib): the bootloader computes the same.
pub fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &b in data {
        crc ^= u32::from(b);
        for _ in 0..8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0xEDB8_8320 } else { crc >> 1 };
        }
    }
    !crc
}

/// The subnet mask for `prefix` bits.
pub fn mask(prefix: u8) -> [u8; 4] {
    let m = if prefix == 0 { 0 } else { u32::MAX << (32 - u32::from(prefix.min(32))) };
    m.to_be_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec() -> Record {
        Record {
            state: State::Start,
            attempts: 0,
            ip: [2, 14, 66, 63],
            prefix: 8,
            gateway: [0; 4],
            mac: [0x28, 0x05, 0xa5, 0x0e, 0x42, 0x3f],
            token: *b"0123456789abcdef",
        }
    }

    #[test]
    fn round_trip() {
        for state in [State::Start, State::Writing, State::Written] {
            let r = Record { state, attempts: 3, ..rec() };
            assert_eq!(Record::decode(&r.encode()), Some(r));
        }
    }

    #[test]
    fn rejects() {
        assert_eq!(Record::decode(&[0xFF; LEN]), None, "erased");
        let mut b = rec().encode();
        b[9] ^= 1;
        assert_eq!(Record::decode(&b), None, "damaged");
        let mut b = rec().encode();
        b[4] = 2;
        assert_eq!(Record::decode(&b), None, "other version");
        assert_eq!(Record::decode(&rec().encode()[..LEN - 1]), None, "short");
    }

    #[test]
    fn crc_matches_zlib() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }

    #[test]
    fn layout_fits_the_slot() {
        assert!(UPDATER + UPDATER_MAX <= RECORD);
        assert_eq!(RECORD % 0x1000, 0);
        assert_eq!(UPDATER % 0x1000, 0);
        assert!(RECORD + 0x1000 <= 0x18_0000);
        assert!(NOTE as usize >= LEN && NOTE as usize + NOTE_MAX <= 0x1000);
    }

    #[test]
    fn masks() {
        assert_eq!(mask(8), [255, 0, 0, 0]);
        assert_eq!(mask(24), [255, 255, 255, 0]);
        assert_eq!(mask(0), [0; 4]);
        assert_eq!(mask(32), [255; 4]);
    }
}
