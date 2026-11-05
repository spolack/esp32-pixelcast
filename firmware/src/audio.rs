//! The microphone as a DMX source: an I2S MEMS mic (SPH0645 on this board)
//! on I2S0, analysed by `pixelcast_core::audio` and sent as one universe
//! 44 times a second (channel map in docs/api.md). Runs on the network core
//! as an ordinary task; the work per wake-up is one DMA chunk (~2.7 ms of
//! audio), well below a millisecond.
//!
//! The packets leave through the receive task's UDP socket ([`TX`]), so
//! Art-Net goes out from port 6454 as the protocol expects.

use core::cell::Cell;
use core::sync::atomic::{AtomicU32, Ordering};

use embassy_sync::{
    blocking_mutex::{Mutex, raw::CriticalSectionRawMutex},
    signal::Signal,
};
use embassy_time::{Duration, Instant, Timer, with_timeout};
use esp_hal::{
    dma::{DmaDescriptor, DmaRxStreamBuf, aligned::DmaAlignedMut},
    gpio::AnyPin,
    i2s::{
        master::{Channels, DataFormat, I2s, I2sRx, TdmConfig},
        pdm::{PdmConfig, PdmRxConfig, PdmSlotMode},
    },
    peripherals::{DMA_I2S0, GPIO3, I2S0},
    time::Rate,
};
use log::{info, warn};
use pixelcast_core::audio::{self as an, Analyzer, Snapshot, Status};

use crate::settings::{self, MIC_PINS, MicType};

/// I2S rate: 48 kHz keeps the SPH0645's clock (64 bits a frame) in its
/// 2.048..4.096 MHz range; decimated by 3 to `an::RATE`.
const I2S_RATE: u32 = 48_000;
const DECIMATE: u32 = I2S_RATE / an::RATE;

/// I2S0 and the GPIOs the mic may use (`settings::Mic` picks them).
pub struct Mic {
    pub i2s: I2S0<'static>,
    pub dma: DMA_I2S0<'static>,
    /// MCLK: the ESP32 has it on GPIO 0, 1 or 3 only; 0 is BOOT and 1 the
    /// serial log here.
    pub mclk: GPIO3<'static>,
    /// The free GPIOs, in `settings::MIC_PINS` order.
    pub pins: [Option<AnyPin<'static>>; MIC_PINS.len()],
}

impl Mic {
    fn take(&mut self, gpio: u8) -> Option<AnyPin<'static>> {
        let i = MIC_PINS.iter().position(|&p| p == gpio)?;
        self.pins[i].take()
    }
}

/// The wiring the capture started with (type and pins apply after a
/// restart), for `/api/config`.
static RUNNING: Mutex<CriticalSectionRawMutex, Cell<Option<settings::Mic>>> = Mutex::new(Cell::new(None));

pub fn running_mic() -> Option<settings::Mic> {
    RUNNING.lock(Cell::get)
}

/// One frame of the audio universe, for the receive task to send
/// (`rx::send_audio` builds the packet from the settings in effect).
#[derive(Clone, Copy)]
pub struct Frame {
    pub channels: [u8; an::CHANNELS],
    pub sequence: u8,
}

/// The latest frame (a newer one replaces an unsent one).
pub static TX: Signal<CriticalSectionRawMutex, Frame> = Signal::new();

/// The DMA ring: 2 KB in four 512-byte chunks (~10.7 ms of mono 32-bit
/// audio, a chunk every ~2.7 ms), in the RAM the bootloader used (the main
/// stack has no room to spare).
const RING: usize = 2048;
#[esp_hal::ram(reclaimed)]
static mut RING_MEM: core::mem::MaybeUninit<[u32; RING / 4]> = core::mem::MaybeUninit::uninit();
static mut DESCRIPTORS: [DmaDescriptor; 4] = [DmaDescriptor::EMPTY; 4];

/// The ring; called once (`run` runs once).
fn ring() -> Option<DmaRxStreamBuf> {
    // SAFETY: only this call takes the two statics; the memory is zeroed
    // before a reference to it exists.
    let (buf, desc) = unsafe {
        let p = core::ptr::addr_of_mut!(RING_MEM).cast::<u8>();
        core::ptr::write_bytes(p, 0, RING);
        (core::slice::from_raw_parts_mut(p, RING), &mut *core::ptr::addr_of_mut!(DESCRIPTORS))
    };
    DmaRxStreamBuf::new(DmaAlignedMut::new(&mut desc[..]).ok()?, DmaAlignedMut::new(buf).ok()?).ok()
}

/// The latest snapshot, for `/api/audio`; `None` while audio is off.
static LAST: Mutex<CriticalSectionRawMutex, Cell<Option<Snapshot>>> = Mutex::new(Cell::new(None));
/// Restarts after the ring ran full (the task came too late, e.g. during a
/// flash write or a burst of Art-Net: the DMA then stops), for `/api/audio`.
static RESTARTS: AtomicU32 = AtomicU32::new(0);

pub fn restarts() -> u32 {
    RESTARTS.load(Ordering::Relaxed)
}

/// Uptime ms until which the beat channel is forced on ("simulate beat").
static SIMULATE_UNTIL: AtomicU32 = AtomicU32::new(0);

pub fn snapshot() -> Option<Snapshot> {
    LAST.lock(Cell::get)
}


/// Sends a beat (pulse and onsets) for 80 ms, to test a console's mapping.
pub fn simulate_beat() {
    SIMULATE_UNTIL.store(Instant::now().as_millis() as u32 + 80, Ordering::Relaxed);
}

/// Waits until audio is switched on, then captures, analyses and sends for
/// good (switching it off stops the sending and clears the values).
pub async fn run(pins: Mic) -> ! {
    while !settings::with(|s| s.audio.on) {
        Timer::after(Duration::from_millis(500)).await;
    }
    let mic = settings::with(|s| s.audio.mic);
    RUNNING.lock(|c| c.set(Some(mic)));
    let Some(rx) = start(pins, &mic) else {
        idle_fault().await
    };
    // PDM delivers 16-bit PCM, I2S 32-bit words (24 bits of data at the top).
    let width = if mic.kind == MicType::Pdm { 2 } else { 4 };
    let Some(buf) = ring() else {
        warn!("audio: DMA ring setup failed");
        idle_fault().await
    };
    let mut transfer = match rx.read(buf) {
        Ok(t) => t,
        Err((e, _, _)) => {
            warn!("audio: I2S start failed: {:?}", e);
            idle_fault().await
        }
    };
    info!("audio: capturing from a {} mic at {} Hz", mic.kind.key(), I2S_RATE);

    let mut analyzer = Analyzer::new();
    let mut sequence = 0u8;
    let started = Instant::now();
    let mut last_data = Instant::now();
    let mut next_fault_frame = Instant::now();
    let (mut acc, mut n_acc) = (0i32, 0u32);
    // The last read found the ring full: the DMA has stopped.
    let mut overflow = false;
    loop {
        // A timeout only means nothing new: `got` stays 0 then.
        if !overflow {
            let _ = with_timeout(Duration::from_millis(20), transfer.wait_for_available_async()).await;
        }
        let audio = settings::with(|s| s.audio);
        // Whole samples, processed in place in the ring.
        let available = transfer.peek().len();
        let got = available - available % width;
        let full = available >= RING;
        if !audio.on {
            transfer.consume(got);
            overflow = full;
            LAST.lock(|c| c.set(None));
            continue;
        }
        if got == 0 && (overflow || last_data.elapsed() >= Duration::from_millis(20)) {
            // The ring ran full and the DMA stopped (or nothing came for
            // 20 ms): start it again. The audio has a gap of the time the
            // task was away.
            overflow = false;
            let (rx, buf) = transfer.stop();
            let (d, b) = buf.split();
            let restarted = DmaRxStreamBuf::new(d, b).ok().and_then(|buf| rx.read(buf).ok());
            let Some(t) = restarted else {
                warn!("audio: I2S restart failed");
                idle_fault().await
            };
            transfer = t;
            RESTARTS.fetch_add(1, Ordering::Relaxed);
        }
        if got == 0 {
            // No samples for half a second: the mic or its clock is gone.
            if last_data.elapsed() > Duration::from_millis(500) && Instant::now() >= next_fault_frame {
                next_fault_frame = Instant::now() + Duration::from_millis(1000 / u64::from(an::FRAME_HZ));
                publish(&mut sequence, Snapshot::empty(Status::Fault));
            }
            continue;
        }
        last_data = Instant::now();
        // The first 250 ms: the mic settles after its clock starts.
        if started.elapsed() < Duration::from_millis(250) {
            transfer.consume(got);
            overflow = full;
            continue;
        }
        for w in transfer.peek()[..got].chunks_exact(width) {
            // As signed 24-bit: I2S has the data at the top of the word.
            acc += if width == 4 {
                i32::from_le_bytes([w[0], w[1], w[2], w[3]]) >> 8
            } else {
                i32::from(i16::from_le_bytes([w[0], w[1]])) << 8
            };
            n_acc += 1;
            if n_acc == DECIMATE {
                let x = acc / DECIMATE as i32;
                (acc, n_acc) = (0, 0);
                if let Some(s) = analyzer.push(x, &audio.params) {
                    publish(&mut sequence, s);
                }
            }
        }
        transfer.consume(got);
        overflow = full;
    }
}

/// Sets up I2S0 for `mic`: standard I2S (optionally with MCLK) or PDM, one
/// slot only (half the data of stereo, so the ring holds twice the time).
fn start(mut p: Mic, mic: &settings::Mic) -> Option<I2sRx<'static, esp_hal::Async>> {
    if let Err(e) = mic.validate() {
        warn!("audio: {}", e);
        return None;
    }
    let (din, ws) = (p.take(mic.din)?, p.take(mic.ws)?);
    let rate = Rate::from_hz(I2S_RATE);
    let rx = match mic.kind {
        MicType::I2s | MicType::I2sMclk => {
            let bclk = p.take(mic.bclk)?;
            // The ESP32 swaps the slots: a mic with SEL to GND (left)
            // arrives as the right one.
            let slot = if mic.sel_high { Channels::LEFT } else { Channels::RIGHT };
            let config = TdmConfig::new_tdm_philips()
                .with_sample_rate(rate)
                .with_data_format(DataFormat::Data32Channel32)
                .with_channels(slot);
            let mut i2s = I2s::new(p.i2s, p.dma, config).inspect_err(|e| warn!("audio: I2S setup failed: {:?}", e)).ok()?;
            if mic.kind == MicType::I2sMclk {
                i2s = i2s.with_mclk(p.mclk);
            }
            i2s.into_async().i2s_rx.with_bclk(bclk).with_ws(ws).with_din(din).build()
        }
        MicType::Pdm => {
            let config = PdmConfig::rx_only(PdmRxConfig::new_pcm_default(rate, PdmSlotMode::Mono));
            let i2s = I2s::new_pdm(p.i2s, p.dma, config).inspect_err(|e| warn!("audio: PDM setup failed: {:?}", e)).ok()?;
            i2s.into_async().i2s_rx.with_clk(ws).with_din(din).build()
        }
    };
    Some(rx)
}

/// I2S could not start: report a mic fault while audio is on.
async fn idle_fault() -> ! {
    let mut sequence = 0u8;
    loop {
        Timer::after(Duration::from_millis(1000 / u64::from(an::FRAME_HZ))).await;
        if settings::with(|s| s.audio.on) {
            publish(&mut sequence, Snapshot::empty(Status::Fault));
        } else {
            LAST.lock(|c| c.set(None));
        }
    }
}

fn publish(sequence: &mut u8, mut s: Snapshot) {
    if Instant::now().as_millis() as u32 <= SIMULATE_UNTIL.load(Ordering::Relaxed) {
        s.beat = true;
        s.onsets[0] = true;
    }
    LAST.lock(|c| c.set(Some(s)));
    // Art-Net: 1..=255 (0 means "no sequencing"); sACN takes the same.
    *sequence = sequence.wrapping_add(1).max(1);
    TX.signal(Frame { channels: an::encode(&s), sequence: *sequence });
}
