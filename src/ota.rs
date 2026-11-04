//! Firmware updates into the inactive app slot, with a trial boot and rollback.
//!
//! Flow:
//! 1. An upload is streamed into the app slot that is not running.
//! 2. The written image is read back and its appended SHA-256 is checked.
//! 3. A trial record is written to the `bootstate` partition, then the new
//!    slot is selected in `otadata`, and the board restarts.
//! 4. On the first boot of the new image, [`boot_check`] marks the trial as
//!    started. Once the firmware is healthy, [`confirm`] clears the record.
//! 5. If the new image crashes, hangs (watchdog) or loses power before it
//!    confirms, the next boot finds the started trial and switches back.
//!
//! The rollback is done by the firmware itself, so it works whether or not the
//! bootloader was built with its own rollback support. It cannot help if the
//! new image dies before [`boot_check`] runs; the image checks below make that
//! unlikely, and the bootloader rejects images that fail its own checks.

use esp_bootloader_esp_idf::{
    ota::OtaImageState,
    ota_updater::OtaUpdater,
    partitions::{
        self, AppPartitionSubType, FlashStorage, PARTITION_TABLE_MAX_LEN, PartitionEntry,
        PartitionType,
    },
};
use log::{error, info, warn};
use sha2::{Digest, Sha256};

pub use image::{ImageError, Kind, check_header};

pub const SECTOR: usize = 4096;

const BOOTSTATE_LABEL: &str = "bootstate";
const TRIAL_MAGIC: [u8; 4] = *b"OTAT";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// The flash has no OTA layout (old partition table): flash once over USB.
    NoOtaLayout,
    Image(ImageError),
    TooLarge,
    Flash,
    Verify,
}

impl From<partitions::Error> for Error {
    fn from(_: partitions::Error) -> Self {
        Error::Flash
    }
}

/// The partitions relevant to updates.
struct Layout {
    booted: Option<PartitionEntry>,
    /// The OTA app slot that is not running.
    target: Option<PartitionEntry>,
    bootstate: Option<PartitionEntry>,
}

/// True if the partition library can decode this entry. It panics on types
/// it does not know, so a foreign table is checked before anything else
/// (including `OtaUpdater::new`) looks at it.
fn known(p: &PartitionEntry) -> bool {
    let sub = p.raw_subtype();
    match p.raw_type() {
        0 => sub == 0x00 || (0x10..=0x20).contains(&sub),
        1 => sub <= 0x06 || (0x81..=0x83).contains(&sub),
        2 | 3 => sub <= 1,
        _ => false,
    }
}

fn layout(flash: &mut FlashStorage<'_>) -> Result<Layout, Error> {
    let mut buf = [0u8; PARTITION_TABLE_MAX_LEN];
    let pt = partitions::read_partition_table(flash, &mut buf)?;
    if !pt.iter().all(|p| known(&p)) {
        return Err(Error::NoOtaLayout);
    }
    let booted = pt.booted_partition()?;
    // Without knowing the running slot, any slot could be the running one:
    // never write anything then.
    if booted.is_none() {
        return Err(Error::NoOtaLayout);
    }
    let mut target = None;
    let mut bootstate = None;
    for p in pt.iter() {
        match p.partition_type() {
            PartitionType::App(AppPartitionSubType::Ota0 | AppPartitionSubType::Ota1) => {
                let running = booted.is_some_and(|b| b.offset() == p.offset());
                if !running && target.is_none() {
                    target = Some(p);
                }
            }
            _ if p.label_as_str() == BOOTSTATE_LABEL => bootstate = Some(p),
            _ => {}
        }
    }
    Ok(Layout {
        booted,
        target,
        bootstate,
    })
}

fn app_subtype(p: &PartitionEntry) -> Option<AppPartitionSubType> {
    match p.partition_type() {
        PartitionType::App(s) => Some(s),
        _ => None,
    }
}

/// Selects `slot` in `otadata` and sets its image state.
fn select(
    flash: &mut FlashStorage<'_>,
    slot: AppPartitionSubType,
    state: OtaImageState,
) -> Result<(), Error> {
    let mut buf = [0u8; PARTITION_TABLE_MAX_LEN];
    let mut updater = OtaUpdater::new(flash, &mut buf).map_err(|_| Error::NoOtaLayout)?;
    let mut ota = updater.ota_data()?;
    if ota.current_app_partition().is_err() {
        // A damaged entry (e.g. power lost while it was written) makes every
        // otadata operation fail. Erase both entries and start over; the
        // bootloader treats that as "boot the first app slot" meanwhile.
        warn!("ota: otadata unreadable; resetting it");
        ota.set_current_app_partition(AppPartitionSubType::Factory)?;
    }
    // From an empty otadata ("Factory"), the library computes sequence 0 for
    // ota_1, which only works through an integer wraparound (and panics in
    // builds with overflow checks). Select ota_0 first (sequence 1); ota_1
    // then gets sequence 2. A power loss in between boots ota_0, which the
    // trial logic already treats as "the update did not start".
    if slot != AppPartitionSubType::Ota0
        && ota.current_app_partition()? == AppPartitionSubType::Factory
    {
        ota.set_current_app_partition(AppPartitionSubType::Ota0)?;
    }
    ota.set_current_app_partition(slot)?;
    ota.set_current_ota_state(state)?;
    NEXT.store(slot_code(slot), core::sync::atomic::Ordering::Relaxed);
    Ok(())
}

/// The slot `otadata` selects for the next start (see [`slot_code`]): read
/// at start-up, then kept current by [`select`], its only writer here.
static NEXT: core::sync::atomic::AtomicU8 = core::sync::atomic::AtomicU8::new(0);

fn slot_code(s: AppPartitionSubType) -> u8 {
    match s {
        AppPartitionSubType::Ota0 => 1,
        AppPartitionSubType::Ota1 => 2,
        _ => 0,
    }
}

/// Reads which slot `otadata` selects (once at start-up).
pub fn read_next_slot(flash: &mut FlashStorage<'_>) {
    let code = selected(flash).map_or(0, slot_code);
    NEXT.store(code, core::sync::atomic::Ordering::Relaxed);
}

/// The slot the next start boots: "ota_0", "ota_1", or None (otadata
/// selects none: the bootloader then boots ota_0).
pub fn next_slot() -> Option<&'static str> {
    match NEXT.load(core::sync::atomic::Ordering::Relaxed) {
        1 => Some("ota_0"),
        2 => Some("ota_1"),
        _ => None,
    }
}

/// The otadata state of the selected entry.
fn running_state(flash: &mut FlashStorage<'_>) -> Option<OtaImageState> {
    let mut buf = [0u8; PARTITION_TABLE_MAX_LEN];
    let mut updater = OtaUpdater::new(flash, &mut buf).ok()?;
    updater.ota_data().ok()?.current_ota_state().ok()
}

/// The slot `otadata` currently selects; `Factory` when it selects none.
fn selected(flash: &mut FlashStorage<'_>) -> Result<AppPartitionSubType, Error> {
    let mut buf = [0u8; PARTITION_TABLE_MAX_LEN];
    let mut updater = OtaUpdater::new(flash, &mut buf).map_err(|_| Error::NoOtaLayout)?;
    Ok(updater.ota_data()?.current_app_partition()?)
}

// ── Trial record ────────────────────────────────────────────────────────────
// 12 bytes at the start of `bootstate`: magic, offset of the slot on trial,
// number of boots of that slot so far.

struct Trial {
    slot_offset: u32,
    boots: u32,
}

fn read_trial(flash: &mut FlashStorage<'_>, bs: PartitionEntry) -> Option<Trial> {
    let mut raw = [0u8; 12];
    bs.as_flash_region(flash).read(0, &mut raw).ok()?;
    if raw[..4] != TRIAL_MAGIC {
        return None;
    }
    Some(Trial {
        slot_offset: u32::from_le_bytes([raw[4], raw[5], raw[6], raw[7]]),
        boots: u32::from_le_bytes([raw[8], raw[9], raw[10], raw[11]]),
    })
}

fn write_trial(flash: &mut FlashStorage<'_>, bs: PartitionEntry, t: &Trial) -> Result<(), Error> {
    let mut raw = [0u8; 12];
    raw[..4].copy_from_slice(&TRIAL_MAGIC);
    raw[4..8].copy_from_slice(&t.slot_offset.to_le_bytes());
    raw[8..].copy_from_slice(&t.boots.to_le_bytes());
    // `write` erases and rewrites the whole sector.
    bs.as_flash_region(flash).write(0, &raw)?;
    Ok(())
}

fn clear_trial(flash: &mut FlashStorage<'_>, bs: PartitionEntry) -> Result<(), Error> {
    bs.as_flash_region(flash).erase(0, SECTOR as u32)?;
    Ok(())
}

// ── Boot ────────────────────────────────────────────────────────────────────

/// What [`boot_check`] found.
#[derive(Debug, Clone, Copy)]
pub struct BootInfo {
    /// Label of the running slot, e.g. "ota_0".
    pub slot: &'static str,
    /// This is the first boot of a new image; it must call [`confirm`].
    pub trial: bool,
    /// The flash has a usable OTA layout.
    pub ota_available: bool,
}

fn slot_name(p: Option<PartitionEntry>) -> &'static str {
    match p.and_then(|p| app_subtype(&p)) {
        Some(AppPartitionSubType::Ota0) => "ota_0",
        Some(AppPartitionSubType::Ota1) => "ota_1",
        Some(AppPartitionSubType::Factory) => "factory",
        _ => "unknown",
    }
}

/// Runs early at boot. Starts or ends a trial, and rolls back to the previous
/// image if the last trial boot never confirmed. A rollback restarts the chip.
pub fn boot_check(flash: &mut FlashStorage<'_>, _: &crate::core1::Paused) -> BootInfo {
    let l = match layout(flash) {
        Ok(l) => l,
        Err(_) => {
            warn!("ota: cannot read the partition table; updates disabled");
            return BootInfo {
                slot: "unknown",
                trial: false,
                ota_available: false,
            };
        }
    };
    let mut info = BootInfo {
        slot: slot_name(l.booted),
        trial: false,
        ota_available: l.target.is_some() && l.bootstate.is_some(),
    };
    if !info.ota_available {
        warn!("ota: no OTA partitions; flash once over USB to enable updates");
        return info;
    }
    let (Some(bs), Some(booted)) = (l.bootstate, l.booted) else {
        return info;
    };
    let Some(mut trial) = read_trial(flash, bs) else {
        // No update in progress. Make otadata name the running slot: after a
        // USB flash it is erased (selects nothing), and the library's
        // sequence math only works cleanly once a slot is selected. This also
        // repairs a damaged entry.
        if let Some(s) = app_subtype(&booted)
            && s != AppPartitionSubType::Factory
            && selected(flash).ok() != Some(s)
        {
            info!("ota: selecting the running slot {} in otadata", info.slot);
            if let Err(e) = select(flash, s, OtaImageState::Valid) {
                warn!("ota: could not update otadata: {:?}", e);
            }
        } else if matches!(
            running_state(flash),
            Some(OtaImageState::New | OtaImageState::PendingVerify | OtaImageState::Undefined)
        ) {
            // WLED marks this firmware "new" when it installs it (with
            // rollback support), or leaves the state undefined (without, and
            // on its way back). A bootloader with rollback support (e.g. one
            // flashed together with WLED) would count "new" as an
            // unconfirmed trial and skip this slot after the next restart;
            // the in-place update wants a confirmed slot: mark it valid.
            info!("ota: marking the running slot valid in otadata");
            if let Some(s) = app_subtype(&booted)
                && let Err(e) = select(flash, s, OtaImageState::Valid)
            {
                warn!("ota: could not update otadata: {:?}", e);
            }
        }
        return info;
    };

    if trial.slot_offset != booted.offset() {
        // The bootloader refused the new image and started the old one.
        warn!("ota: the updated image did not start; staying on {}", info.slot);
        if let Some(s) = app_subtype(&booted) {
            let _ = select(flash, s, OtaImageState::Valid);
        }
        let _ = clear_trial(flash, bs);
        return info;
    }

    if trial.boots == 0 {
        trial.boots = 1;
        if write_trial(flash, bs, &trial).is_ok() {
            info!("ota: trial boot of the new image in {}", info.slot);
            info.trial = true;
        }
        return info;
    }

    // The previous trial boot crashed, hung or lost power before it confirmed.
    // Select the old image first and only then drop the record: if power
    // fails in between, the record names a slot that no longer boots and is
    // cleared by the branch above on the next start.
    error!("ota: the new image in {} never confirmed; rolling back", info.slot);
    if let Some(prev) = l.target.and_then(|p| app_subtype(&p))
        && select(flash, prev, OtaImageState::Valid).is_ok()
    {
        let _ = clear_trial(flash, bs);
        if crate::crashlog::last_kind() == crate::crashlog::Kind::Planned {
            // `/rollback` restarted on purpose: its record already says so.
            crate::crashlog::planned_keeping_last("Rolled back");
        } else {
            crate::crashlog::planned_keeping_last("Rolled back: the updated image restarted before it was confirmed");
        }
        esp_hal::system::software_reset();
    }
    error!("ota: rollback failed; will retry on the next start");
    info
}

/// Marks the running image as good. Call once the firmware has proved itself.
///
/// Does nothing unless the trial record names the running slot. That keeps
/// it from undoing an update that was installed meanwhile (its record names
/// the other slot).
pub fn confirm(flash: &mut FlashStorage<'_>, _: &crate::core1::Paused) -> Result<(), Error> {
    let l = layout(flash)?;
    let (Some(bs), Some(booted)) = (l.bootstate, l.booted) else {
        return Ok(());
    };
    match read_trial(flash, bs) {
        Some(t) if t.slot_offset == booted.offset() => {}
        _ => return Ok(()),
    }
    // Select first, then clear: if power fails in between, the next start
    // sees a trial with boots=1 and rolls back, which is safe, just wasteful.
    // The other order could leave a bootloader with its own rollback support
    // treating the image as never confirmed.
    if let Some(s) = app_subtype(&booted)
        && s != AppPartitionSubType::Factory
    {
        select(flash, s, OtaImageState::Valid)?;
    }
    clear_trial(flash, bs)?;
    info!("ota: image confirmed");
    Ok(())
}

// ── Update ──────────────────────────────────────────────────────────────────

/// Streams one image into the inactive slot. Feed whole sectors with
/// [`Updater::write_sector`], then call [`Updater::finish`].
pub struct Updater {
    target: PartitionEntry,
    bootstate: PartitionEntry,
    total: u32,
    written: u32,
    /// From the first sector.
    kind: Kind,
    /// Over everything uploaded, to check the written flash against.
    hasher: Sha256,
}

impl Updater {
    /// `total` is the image size in bytes (the HTTP Content-Length).
    pub fn begin(flash: &mut FlashStorage<'_>, _: &crate::core1::Paused, total: u32) -> Result<Self, Error> {
        let l = layout(flash)?;
        let (Some(target), Some(bootstate)) = (l.target, l.bootstate) else {
            return Err(Error::NoOtaLayout);
        };
        if total as usize > target.len() as usize {
            return Err(Error::TooLarge);
        }
        if (total as usize) < image::MIN_LEN {
            return Err(Error::Image(ImageError::TooShort));
        }
        info!(
            "ota: writing {} bytes into {}",
            total,
            slot_name(Some(target))
        );
        Ok(Self {
            target,
            bootstate,
            total,
            written: 0,
            kind: Kind::Ours,
            hasher: Sha256::new(),
        })
    }

    /// Which firmware is being written (known after the first sector).
    pub fn kind(&self) -> Kind {
        self.kind
    }

    pub fn written(&self) -> u32 {
        self.written
    }

    /// Writes the next chunk. Every chunk but the last must be [`SECTOR`]
    /// bytes, so each write lands on a sector boundary.
    pub fn write_sector(&mut self, flash: &mut FlashStorage<'_>, _: &crate::core1::Paused, data: &[u8]) -> Result<(), Error> {
        if self.written == 0 {
            self.kind = check_header(data).map_err(Error::Image)?;
        }
        if self.written + data.len() as u32 > self.total {
            return Err(Error::TooLarge);
        }
        self.target
            .as_flash_region(flash)
            .write(self.written, data)?;
        self.hasher.update(data);
        self.written += data.len() as u32;
        Ok(())
    }

    /// Verifies the image in flash and selects it for the next boot.
    pub fn finish(self, flash: &mut FlashStorage<'_>, _: &crate::core1::Paused) -> Result<(), Error> {
        if self.written != self.total {
            return Err(Error::Verify);
        }
        // What is in flash is what was uploaded...
        let uploaded: [u8; 32] = self.hasher.finalize().into();
        if hash_range(flash, self.target, self.total)? != uploaded {
            error!("ota: the written image differs from the upload");
            return Err(Error::Verify);
        }
        // ...and the upload is a complete, intact image.
        verify_stored(flash, self.target, self.total)?;
        // An in-place update request left in this slot (a power cut at the
        // wrong moment) must not start the updater once it becomes active.
        clear_in_place_record(flash, self.target);
        let slot = app_subtype(&self.target).ok_or(Error::NoOtaLayout)?;
        if self.kind == Kind::Other {
            // No trial: this firmware does not run to confirm it. Drop any
            // old record so it cannot roll back once this firmware returns.
            // "new" lets a bootloader with rollback catch a crash before WLED
            // marks itself valid.
            let _ = clear_trial(flash, self.bootstate);
            select(flash, slot, OtaImageState::New)?;
            info!("ota: other firmware in {} selected for the next boot", slot_name(Some(self.target)));
            return Ok(());
        }
        // Record first: if power fails before otadata changes, the record
        // names a slot that does not boot and is cleared on the next start.
        write_trial(
            flash,
            self.bootstate,
            &Trial {
                slot_offset: self.target.offset(),
                boots: 0,
            },
        )?;
        select(flash, slot, OtaImageState::New)?;
        info!("ota: {} selected for the next boot", slot_name(Some(self.target)));
        Ok(())
    }
}

/// SHA-256 of the first `len` bytes of `p`.
fn hash_range(flash: &mut FlashStorage<'_>, p: PartitionEntry, len: u32) -> Result<[u8; 32], Error> {
    let mut region = p.as_flash_region(flash);
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 1024];
    let mut off = 0;
    while off < len {
        let n = (len - off).min(buf.len() as u32) as usize;
        region.read(off, &mut buf[..n])?;
        hasher.update(&buf[..n]);
        off += n as u32;
    }
    Ok(hasher.finalize().into())
}

/// Checks the image stored in `p` (`total` bytes, see `stored_image_len`)
/// the way the bootloader does: the appended SHA-256 if it has one, and the
/// checksum byte over the segment data.
fn verify_stored(flash: &mut FlashStorage<'_>, p: PartitionEntry, total: u32) -> Result<(), Error> {
    let mut region = p.as_flash_region(flash);
    let mut head = [0u8; 24];
    region.read(0, &mut head)?;
    let hashed = head[23] == 1;
    // Checksum: 0xEF XOR every segment data byte, in the last byte of the
    // 16-byte block after the segments.
    let mut ck = 0xEFu8;
    let mut off = 24u32;
    let mut buf = [0u8; 1024];
    for _ in 0..head[1] {
        let mut seg = [0u8; 8];
        region.read(off, &mut seg)?;
        let len = u32::from_le_bytes([seg[4], seg[5], seg[6], seg[7]]);
        off += 8;
        let end = off.checked_add(len).ok_or(Error::Verify)?;
        if end > total {
            return Err(Error::Verify);
        }
        while off < end {
            let n = (end - off).min(buf.len() as u32) as usize;
            region.read(off, &mut buf[..n])?;
            ck = buf[..n].iter().fold(ck, |c, b| c ^ b);
            off += n as u32;
        }
    }
    let ck_at = (off + 1).div_ceil(16) * 16 - 1;
    let mut stored = [0u8; 1];
    region.read(ck_at, &mut stored)?;
    if stored[0] != ck {
        error!("ota: image checksum mismatch");
        return Err(Error::Verify);
    }
    if hashed {
        let body = ck_at + 1;
        let mut expected = [0u8; 32];
        region.read(body, &mut expected)?;
        if hash_range(flash, p, body)? != expected {
            error!("ota: SHA-256 mismatch");
            return Err(Error::Verify);
        }
    }
    Ok(())
}

// ── WLED in the other slot (dual boot) ─────────────────────────────────────
// The layout is WLED's (two app slots): each firmware installs into the other
// slot, so WLED's update page can bring this firmware back, and this one's
// can install WLED. While WLED is in the other slot, this firmware can start
// it; installing an update of this firmware replaces it.

/// The other app slot, if it holds a complete image of another firmware.
fn other_firmware(flash: &mut FlashStorage<'_>) -> Option<(PartitionEntry, u32)> {
    let target = layout(flash).ok()?.target?;
    let total = stored_image_len(flash, target)?;
    let mut head = [0u8; image::MIN_LEN];
    target.as_flash_region(flash).read(0, &mut head).ok()?;
    (check_header(&head) == Ok(Kind::Other)).then_some((target, total))
}

/// Length of the ESP image stored at the start of `p` (header, segments,
/// checksum, appended hash), or `None` if it does not look like one.
fn stored_image_len(flash: &mut FlashStorage<'_>, p: PartitionEntry) -> Option<u32> {
    let mut region = p.as_flash_region(flash);
    let mut head = [0u8; 24];
    region.read(0, &mut head).ok()?;
    if head[0] != 0xE9 || head[1] == 0 || head[1] > 16 {
        return None; // not an image
    }
    let mut off = 24u32;
    for _ in 0..head[1] {
        let mut seg = [0u8; 8];
        region.read(off, &mut seg).ok()?;
        let len = u32::from_le_bytes([seg[4], seg[5], seg[6], seg[7]]);
        off = off.checked_add(8)?.checked_add(len)?;
        if off > p.len() {
            return None;
        }
    }
    // Checksum byte at the end of a 16-byte block, then the SHA-256 (if any).
    let total = (off + 1).div_ceil(16) * 16 + if head[23] == 1 { 32 } else { 0 };
    (total <= p.len()).then_some(total)
}

/// Another firmware (WLED) looks present in the other slot (cheap: header
/// and segments only).
pub fn wled_present(flash: &mut FlashStorage<'_>) -> bool {
    other_firmware(flash).is_some()
}

/// Checks the image in the other slot in full (checksum, SHA-256) and
/// selects it for the next boot; the caller restarts. `only_other`: refuse
/// anything but another firmware (WLED).
/// - Any firmware started this way stays: no trial. It marks its slot valid
///   when it starts (WLED's Arduino core and [`boot_check`] do), so the
///   bootloader's rollback only covers a crash before that. The way back is
///   the BOOT button (or three clicks), or the other firmware's own start
///   button. Uploads still run on trial (see [`confirm`]): nobody may be at
///   the board then.
pub fn boot_other(flash: &mut FlashStorage<'_>, _: &crate::core1::Paused, only_other: bool) -> Result<Kind, Error> {
    let l = layout(flash)?;
    let (Some(p), Some(bs)) = (l.target, l.bootstate) else {
        return Err(Error::NoOtaLayout);
    };
    let total = stored_image_len(flash, p).ok_or(Error::Image(ImageError::NotAnImage))?;
    let mut head = [0u8; image::MIN_LEN];
    p.as_flash_region(flash).read(0, &mut head)?;
    let kind = check_header(&head).map_err(Error::Image)?;
    if only_other && kind != Kind::Other {
        return Err(Error::Image(ImageError::NotAnImage));
    }
    verify_stored(flash, p, total)?;
    let slot = app_subtype(&p).ok_or(Error::NoOtaLayout)?;
    let _ = clear_trial(flash, bs);
    // "new": a bootloader with rollback boots it once as "pending verify"
    // and returns here if it restarts before marking itself valid.
    select(flash, slot, OtaImageState::New)?;
    info!("ota: {} selected for the next boot ({:?})", slot_name(Some(p)), kind);
    Ok(kind)
}

// ── What the slots hold (for the update page) ───────────────────────────────

/// One app slot.
#[derive(Debug, Clone)]
pub struct SlotInfo {
    /// "ota_0" or "ota_1".
    pub label: &'static str,
    pub running: bool,
    /// None: no complete image of this chip.
    pub kind: Option<Kind>,
    /// "esp32-pixelcast", "WLED" or another firmware's project name.
    pub firmware: heapless::String<32>,
    pub version: heapless::String<32>,
    /// Build date and time (this firmware only; others carry their SDK's).
    pub built: heapless::String<32>,
}

/// The app slots in flash order, read once at start-up.
pub fn slots(flash: &mut FlashStorage<'_>) -> heapless::Vec<SlotInfo, 2> {
    let mut out = heapless::Vec::new();
    let Ok(l) = layout(flash) else {
        return out;
    };
    let mut ps: heapless::Vec<PartitionEntry, 2> = l.booted.into_iter().chain(l.target).collect();
    ps.sort_unstable_by_key(|p| p.offset());
    for p in ps {
        let running = l.booted.is_some_and(|b| b.offset() == p.offset());
        let _ = out.push(slot_info(flash, p, running));
    }
    out
}

fn slot_info(flash: &mut FlashStorage<'_>, p: PartitionEntry, running: bool) -> SlotInfo {
    let mut s = SlotInfo {
        label: slot_name(Some(p)),
        running,
        kind: None,
        firmware: heapless::String::new(),
        version: heapless::String::new(),
        built: heapless::String::new(),
    };
    let mut head = [0u8; 24 + 8 + 128];
    if stored_image_len(flash, p).is_none() || p.as_flash_region(flash).read(0, &mut head).is_err() {
        return s;
    }
    let Ok(kind) = check_header(&head) else {
        return s;
    };
    s.kind = Some(kind);
    let desc = &head[24 + 8..];
    let field = |o: usize, n: usize| text(&desc[o..o + n]);
    match kind {
        Kind::Ours => {
            let _ = s.firmware.push_str(field(48, 32));
            let _ = s.version.push_str(field(16, 32));
            let _ = core::fmt::write(&mut s.built, format_args!("{} {}", field(96, 16), field(80, 16)));
        }
        Kind::Other => {
            // WLED's app descriptor names its SDK (arduino-lib-builder).
            // WLED 16+ carries its metadata block (magic "WSTS", u32
            // version, 48-byte version, 48-byte release name); older ones
            // only a string ("... WLED v14.7.2-mdev").
            let mut m = [0u8; 4 + 48 + 48];
            let mut v = [0u8; 32];
            if find_after(flash, p, &0x5753_5453u32.to_le_bytes(), &mut m).is_some() {
                let _ = s.firmware.push_str("WLED");
                let _ = s.version.push_str(text(&m[4..52]));
                // Its build ("ESP32_W5500"), when it fits.
                let rel = text(&m[52..100]);
                if !rel.is_empty() && s.version.len() + 1 + rel.len() <= s.version.capacity() {
                    let _ = s.version.push_str(" ");
                    let _ = s.version.push_str(rel);
                }
            } else if let Some(n) = find_after(flash, p, b"WLED v", &mut v) {
                let _ = s.firmware.push_str("WLED");
                let _ = s.version.push_str(text(&v[..n]));
            } else {
                let _ = s.firmware.push_str(field(48, 32));
                let _ = s.version.push_str(field(16, 32));
            }
        }
    }
    s
}

/// The printable ASCII up to the first other byte.
fn text(b: &[u8]) -> &str {
    let n = b.iter().position(|c| !(0x20..0x7f).contains(c)).unwrap_or(b.len());
    core::str::from_utf8(&b[..n]).unwrap_or("")
}

/// Searches the first 64 KB of `p` for `pat` and copies what follows into
/// `out` (at most 120 bytes); returns its printable length.
fn find_after(flash: &mut FlashStorage<'_>, p: PartitionEntry, pat: &[u8], out: &mut [u8]) -> Option<usize> {
    const SPAN: u32 = 64 * 1024;
    const CHUNK: usize = 1024;
    let keep = pat.len() + out.len();
    let mut buf = [0u8; CHUNK + 128];
    if keep > 128 {
        return None;
    }
    let mut region = p.as_flash_region(flash);
    let mut filled = 0;
    let mut off = 0u32;
    while off < SPAN.min(p.len()) {
        region.read(off, &mut buf[filled..filled + CHUNK]).ok()?;
        let end = filled + CHUNK;
        if let Some(i) = buf[..end].windows(pat.len()).position(|w| w == pat)
            && i + keep <= end
        {
            let n = out.len();
            out.copy_from_slice(&buf[i + pat.len()..i + pat.len() + n]);
            return Some(text(&out[..]).len());
        }
        // Keep the tail, so a match across chunks is found.
        buf.copy_within(end - keep..end, 0);
        filled = keep;
        off += CHUNK as u32;
    }
    None
}

/// Checks on the image header, independent of the hardware.
pub mod image {
    /// Header (24) + first segment header (8) + app descriptor up to the
    /// project name (80) + appended hash (32).
    pub const MIN_LEN: usize = 24 + 8 + 80 + 32;

    const IMAGE_MAGIC: u8 = 0xE9;
    const CHIP_ID_ESP32: u16 = 0x0000;
    const APP_DESC_MAGIC: u32 = 0xABCD_5432;
    /// Offset of `esp_app_desc_t` in the image: right after the first
    /// segment header.
    const APP_DESC: usize = 24 + 8;
    const PROJECT_NAME: usize = APP_DESC + 48;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum ImageError {
        TooShort,
        /// Not an ESP firmware image (e.g. an ELF file or a merged image).
        NotAnImage,
        WrongChip,
        /// The image has no SHA-256 appended, so it cannot be verified.
        NoHash,
        NoAppDescriptor,
    }

    /// Which firmware an image is.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Kind {
        /// This firmware: installed on trial (confirm or roll back).
        Ours,
        /// Another ESP32 firmware (WLED): installed and started as is; it
        /// cannot confirm, and coming back means uploading this one there.
        Other,
    }

    /// Checks the first bytes of an image.
    pub fn check_header(h: &[u8]) -> Result<Kind, ImageError> {
        if h.len() < PROJECT_NAME + 32 {
            return Err(ImageError::TooShort);
        }
        if h[0] != IMAGE_MAGIC {
            return Err(ImageError::NotAnImage);
        }
        if u16::from_le_bytes([h[12], h[13]]) != CHIP_ID_ESP32 {
            return Err(ImageError::WrongChip);
        }
        let magic = u32::from_le_bytes([
            h[APP_DESC],
            h[APP_DESC + 1],
            h[APP_DESC + 2],
            h[APP_DESC + 3],
        ]);
        if magic != APP_DESC_MAGIC {
            return Err(ImageError::NoAppDescriptor);
        }
        let name = &h[PROJECT_NAME..PROJECT_NAME + 32];
        let len = name.iter().position(|&b| b == 0).unwrap_or(32);
        if &name[..len] == env!("CARGO_PKG_NAME").as_bytes() {
            // espflash always appends it; without, something is off.
            if h[23] != 1 {
                return Err(ImageError::NoHash);
            }
            Ok(Kind::Ours)
        } else {
            // WLED (arduino) images carry only the checksum byte; those are
            // verified against the upload's own hash instead.
            Ok(Kind::Other)
        }
    }
}

// ── Moving to the WLED-compatible layout over the network ───────────────────
// Boards flashed with the old table (two 1.9 MB slots) move to
// `partitions.csv` (WLED's layout) without USB. Both have app0 at 0x10000,
// so the firmware running there stays where it is; only the table sector,
// the settings and the trial record move. The one window without a way back
// is the rewrite of the table sector (~50 ms): a power cut then needs USB.

/// The table the old layout was flashed with (`espflash partition-table
/// --to-binary` of the previous partitions.csv) and the new one.
const TABLE_OLD: &[u8] = include_bytes!("partitions-old.bin");
const TABLE_NEW: &[u8] = include_bytes!("partitions-wled.bin");
const TABLE_ADDR: u32 = 0x8000;
/// In the new table: bootstate and storage (settings in sector 0).
const NEW_BOOTSTATE: u32 = 0x3F_B000;
const NEW_STORAGE: u32 = 0x3F_C000;
const NEW_STORAGE_LEN: u32 = 0x4000;
const NEW_APP_LEN: u32 = 0x18_0000;
/// WLED's filesystem (spiffs) in the new table.
const NEW_FS: u32 = 0x31_0000;
/// The old storage partition (settings in sectors 0 and 1, handoff in 2).
const OLD_STORAGE: u32 = 0x3D_1000;

/// Which table is in flash.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TableState {
    Old,
    Wled,
    /// Neither: not touched.
    Unknown,
}

impl TableState {
    pub fn key(self) -> &'static str {
        match self {
            TableState::Old => "old",
            TableState::Wled => "wled",
            TableState::Unknown => "unknown",
        }
    }
}

pub fn table_state(flash: &mut FlashStorage<'_>) -> TableState {
    let mut buf = [0u8; 3072];
    if flash.read(TABLE_ADDR, &mut buf[..TABLE_OLD.len()]).is_err() {
        return TableState::Unknown;
    }
    if buf[..TABLE_NEW.len()] == *TABLE_NEW {
        TableState::Wled
    } else if buf[..TABLE_OLD.len()] == *TABLE_OLD {
        TableState::Old
    } else {
        TableState::Unknown
    }
}

/// Moves to the WLED-compatible layout; `settings_form` is the settings to
/// carry over. Refuses unless everything is as expected. The caller
/// restarts on success.
pub fn migrate_layout(
    flash: &mut FlashStorage<'_>,
    _: &crate::core1::Paused,
    buf: &mut [u8; SECTOR],
    settings_form: &str,
) -> Result<(), &'static str> {
    const _: () = assert!(TABLE_OLD.len() == 3072 && TABLE_NEW.len() == 3072);
    if table_state(flash) != TableState::Old {
        return Err("The partition table is not the expected old one; nothing changed.\n");
    }
    let l = layout(flash).map_err(|_| "Cannot read the partition table; nothing changed.\n")?;
    let booted = l.booted.ok_or("The running slot is unknown; nothing changed.\n")?;
    if booted.offset() != 0x10000 || app_subtype(&booted) != Some(AppPartitionSubType::Ota0) {
        return Err("Run from the first slot (ota_0) first: install this firmware once more, confirm, then retry.\n");
    }
    if selected(flash).ok() != Some(AppPartitionSubType::Ota0) {
        return Err("otadata does not select the running slot; nothing changed.\n");
    }
    if let Some(bs) = l.bootstate
        && read_trial(flash, bs).is_some()
    {
        return Err("An update trial is pending: confirm or roll back first.\n");
    }
    match stored_image_len(flash, booted) {
        Some(n) if n <= NEW_APP_LEN => {}
        _ => return Err("The running image does not fit the new 1.5 MB slot; nothing changed.\n"),
    }
    if settings_form.len() > crate::settings::MAX_LEN {
        return Err("The settings are too large; nothing changed.\n");
    }

    // 1. Settings and trial record at their new places (unused space of the
    //    old storage partition, so the old settings stay intact until the
    //    table changes).
    let n = crate::settings::record_into(buf, settings_form, 1);
    // Erased, not just overwritten: the area may hold older data.
    flash
        .erase(NEW_BOOTSTATE, NEW_STORAGE + NEW_STORAGE_LEN)
        .map_err(|_| "Erasing the new settings place failed; nothing changed.\n")?;
    flash.write(NEW_STORAGE, &buf[..n]).map_err(|_| "Writing the settings failed; nothing changed.\n")?;
    let mut check = [0u8; 64];
    let m = n.min(check.len());
    flash.read(NEW_STORAGE, &mut check[..m]).map_err(|_| "Reading back failed; nothing changed.\n")?;
    if check[..m] != buf[..m] {
        return Err("The settings did not read back; nothing changed.\n");
    }

    // 2. The table: one sector, read back; the old one goes back on a
    //    mismatch.
    warn!("ota: writing the WLED-compatible partition table");
    let written = flash.write(TABLE_ADDR, TABLE_NEW).is_ok();
    let mut back = [0u8; 3072];
    let same = written && flash.read(TABLE_ADDR, &mut back).is_ok() && back[..] == *TABLE_NEW;
    if !same {
        error!("ota: the new table did not read back; restoring the old one");
        let _ = flash.write(TABLE_ADDR, TABLE_OLD);
        return Err("Writing the partition table failed; the old one was written back.\n");
    }
    // The old settings (with the web password) would stay readable inside
    // WLED's future filesystem: erase them now that the table is in place.
    if flash.erase(OLD_STORAGE, OLD_STORAGE + 3 * SECTOR as u32).is_err() {
        warn!("ota: could not erase the old settings");
    }
    // A WLED filesystem from before this firmware (stock table: 0xF0000 at
    // the same place) can survive in the old ota_1; WLED asserts mounting it
    // at the smaller new size and restarts forever. Without its superblocks
    // (blocks 0 and 1) WLED formats a fresh one.
    if flash.erase(NEW_FS, NEW_FS + 2 * SECTOR as u32).is_err() {
        warn!("ota: could not erase the old WLED filesystem");
    }
    info!("ota: WLED-compatible layout written");
    Ok(())
}


// ── Bootloader (tools/bootloader) ───────────────────────────────────────────

/// The bootloader from `tools/bootloader` (rollback, BOOT button slot
/// switch), built there and copied to `tools/bootloader/bootloader.bin`.
const BOOTLOADER: &[u8] = include_bytes!(env!("PXC_BOOTLOADER_BIN"));
const BOOTLOADER_ADDR: u32 = 0x1000;
// It must end before the partition table and be an ESP32 image.
const _: () = assert!(BOOTLOADER.len() <= (TABLE_ADDR - BOOTLOADER_ADDR) as usize && BOOTLOADER[0] == 0xE9);

/// Marker in the bootloader image (tools/bootloader), followed by its
/// feature level: 1 rollback and the BOOT slot switch, 2 the in-place update.
const BOOT_ID: &[u8] = b"pixelcast-bootloader ";
/// Level the in-place update needs.
pub const BOOT_LEVEL_IN_PLACE: u8 = 2;

fn boot_level_in(image: &[u8]) -> Option<u8> {
    let at = image.windows(BOOT_ID.len()).position(|w| w == BOOT_ID)? + BOOT_ID.len();
    let digits = image[at..].iter().take_while(|b| b.is_ascii_digit()).take(3);
    digits.fold(None, |n, &d| Some(n.unwrap_or(0u8).saturating_mul(10).saturating_add(d - b'0')))
}

/// Feature level of the installed bootloader (None: not ours, or older than
/// the marker).
pub fn bootloader_level(flash: &mut FlashStorage<'_>) -> Option<u8> {
    let mut window = [0u8; 512 + 32];
    let end = TABLE_ADDR - BOOTLOADER_ADDR;
    let mut off = 0;
    while off < end {
        let n = window.len().min((end - off) as usize);
        flash.read(BOOTLOADER_ADDR + off, &mut window[..n]).ok()?;
        if let Some(level) = boot_level_in(&window[..n]) {
            return Some(level);
        }
        off += 512; // overlapping windows: a marker across the edge is found too
    }
    None
}

/// Feature level of the bundled bootloader.
pub fn bundled_bootloader_level() -> Option<u8> {
    boot_level_in(BOOTLOADER)
}

/// Whether flash holds exactly the bundled bootloader.
pub fn bootloader_current(flash: &mut FlashStorage<'_>) -> bool {
    let mut chunk = [0u8; 256];
    BOOTLOADER.chunks(chunk.len()).enumerate().all(|(i, want)| {
        let got = &mut chunk[..want.len()];
        flash.read(BOOTLOADER_ADDR + (i * 256) as u32, got).is_ok() && got == want
    })
}

/// Writes the bundled bootloader over the one in flash, read back and
/// rewritten up to three times. A power loss during the write (well under
/// a second) leaves a board that only USB can bring back.
pub fn install_bootloader(
    flash: &mut FlashStorage<'_>,
    _: &crate::core1::Paused,
    buf: &mut [u8; SECTOR],
) -> Result<(), &'static str> {
    if bootloader_current(flash) {
        return Err("This bootloader is already installed.\n");
    }
    for attempt in 1..=3 {
        warn!("ota: writing the bootloader (attempt {})", attempt);
        // Through the RAM buffer: the bundled copy is in flash, which cannot
        // be read while it is being written.
        let mut ok = true;
        for (i, part) in BOOTLOADER.chunks(SECTOR).enumerate() {
            buf[..part.len()].copy_from_slice(part);
            ok &= flash.write(BOOTLOADER_ADDR + (i * SECTOR) as u32, &buf[..part.len()]).is_ok();
        }
        if ok && bootloader_current(flash) {
            info!("ota: bootloader installed");
            return Ok(());
        }
        error!("ota: the bootloader did not read back");
    }
    Err("Writing the bootloader failed and it did not read back. Do not restart: retry, or flash it over USB (tools/bootloader/README.md).\n")
}

// ── In-place update (see core/src/inplace.rs and updater/) ─────────────────

use pixelcast_core::inplace;

/// The updater (`updater/`, built with `updater/build.sh`).
const UPDATER_IMAGE: &[u8] = include_bytes!(env!("PXC_UPDATER_BIN"));
const _: () = assert!(UPDATER_IMAGE.len() as u32 <= inplace::UPDATER_MAX && UPDATER_IMAGE[0] == 0xE9);

/// Where the updater comes up: the address this firmware has now.
pub struct InPlaceAddr {
    pub ip: [u8; 4],
    pub prefix: u8,
    pub gateway: [u8; 4],
    pub mac: [u8; 6],
}

fn clear_in_place_record(flash: &mut FlashStorage<'_>, p: PartitionEntry) {
    let at = p.offset() + inplace::RECORD;
    let mut head = [0u8; inplace::LEN];
    if flash.read(at, &mut head).is_ok() && head.iter().any(|&b| b != 0xFF) && head[..4] == *b"PXCU" {
        warn!("ota: dropping an in-place update request in {}", slot_name(Some(p)));
        let _ = flash.erase(at, at + SECTOR as u32);
    }
}

/// Writes the updater and the request record into the unused end of the
/// running slot. The caller restarts; the bootloader then runs the updater,
/// which takes the new image with `token` at the address in `addr`.
pub fn start_in_place(
    flash: &mut FlashStorage<'_>,
    _: &crate::core1::Paused,
    buf: &mut [u8; SECTOR],
    addr: &InPlaceAddr,
    token: [u8; 16],
) -> Result<(), &'static str> {
    if bootloader_level(flash).is_none_or(|l| l < BOOT_LEVEL_IN_PLACE) {
        return Err("The bootloader that runs the updater is not installed: install it first (Maintenance).\n");
    }
    let l = layout(flash).map_err(|_| "Cannot read the partition table.\n")?;
    let booted = l.booted.ok_or("The running slot is unknown.\n")?;
    let slot = app_subtype(&booted).ok_or("The running slot is unknown.\n")?;
    if !matches!(slot, AppPartitionSubType::Ota0 | AppPartitionSubType::Ota1) || booted.len() < 0x18_0000 {
        return Err("Only possible from an update slot of 1.5 MB.\n");
    }
    if selected(flash).ok() != Some(slot) || !matches!(running_state(flash), Some(OtaImageState::Valid | OtaImageState::Undefined)) {
        return Err("The running firmware is not the confirmed, selected one; restart and retry.\n");
    }
    match stored_image_len(flash, booted) {
        Some(n) if n <= inplace::IMAGE_MAX => {}
        _ => return Err("The running image overlaps the updater's place.\n"),
    }
    let base = booted.offset();

    // 1. The updater, read back. Through the RAM buffer: the bundled copy
    //    lives in flash, which cannot be read while it is written.
    let at = base + inplace::UPDATER;
    for (i, part) in UPDATER_IMAGE.chunks(SECTOR).enumerate() {
        let off = at + (i * SECTOR) as u32;
        buf[..part.len()].copy_from_slice(part);
        flash.write(off, &buf[..part.len()]).map_err(|_| "Writing the updater failed.\n")?;
        let mut back = [0u8; 256];
        for (j, want) in part.chunks(back.len()).enumerate() {
            let got = &mut back[..want.len()];
            if flash.read(off + (j * 256) as u32, got).is_err() || got != want {
                return Err("The updater did not read back.\n");
            }
        }
    }

    // 2. The record, last: without it the bootloader ignores the updater.
    let rec = inplace::Record {
        state: inplace::State::Start,
        attempts: 0,
        ip: addr.ip,
        prefix: addr.prefix,
        gateway: addr.gateway,
        mac: addr.mac,
        token,
    };
    let at = base + inplace::RECORD;
    buf.fill(0xFF); // also clears an old note
    buf[..inplace::LEN].copy_from_slice(&rec.encode());
    flash.write(at, &buf[..]).map_err(|_| "Writing the request failed.\n")?;
    let mut back = [0u8; inplace::LEN];
    if flash.read(at, &mut back).is_err() || inplace::Record::decode(&back) != Some(rec) {
        let _ = flash.erase(at, at + SECTOR as u32);
        return Err("The request did not read back.\n");
    }
    warn!("ota: in-place update requested for {}; restarting into the updater", slot_name(Some(booted)));
    Ok(())
}

/// What the updater left when it gave up (in the running slot), if anything.
pub fn in_place_note(flash: &mut FlashStorage<'_>) -> Option<heapless::String<{ inplace::NOTE_MAX }>> {
    let booted = layout(flash).ok()?.booted?;
    let mut raw = [0u8; inplace::NOTE_MAX];
    flash.read(booted.offset() + inplace::RECORD + inplace::NOTE, &mut raw).ok()?;
    let end = raw.iter().position(|&b| b == 0 || b == 0xFF).unwrap_or(raw.len());
    let text = core::str::from_utf8(&raw[..end]).ok()?;
    (!text.is_empty()).then(|| heapless::String::try_from(text).ok()).flatten()
}

// ── WLED factory reset ─────────────────────────────────────────────────────

/// Resets WLED in the other slot to its factory defaults: erases the start
/// of its filesystem (the `spiffs` partition; LittleFS keeps its superblocks
/// in the first two blocks), so WLED formats it on its next start. It then
/// takes this firmware's setup as its first configuration again
/// (core/src/wled.rs). WLED's settings and presets are gone.
pub fn reset_wled(flash: &mut FlashStorage<'_>, _: &crate::core1::Paused) -> Result<(), &'static str> {
    let mut buf = [0u8; PARTITION_TABLE_MAX_LEN];
    let pt = partitions::read_partition_table(flash, &mut buf).map_err(|_| "Cannot read the partition table.\n")?;
    let fs = pt
        .iter()
        .find(|p| p.label_as_str() == "spiffs" && matches!(p.partition_type(), PartitionType::Data(_)))
        .ok_or("This layout has no WLED filesystem.\n")?;
    if fs.len() < 2 * SECTOR as u32 {
        return Err("This layout has no WLED filesystem.\n");
    }
    flash
        .erase(fs.offset(), fs.offset() + 2 * SECTOR as u32)
        .map_err(|_| "Erasing WLED's filesystem failed.\n")?;
    warn!("ota: WLED filesystem erased (factory defaults at its next start)");
    Ok(())
}
