//! Pin map of the ArtnetNode-ESP32 carrier (ESP32-DevKitC-32D + W5500).
//!
//! Taken from the EasyEDA schematic ("W5500 mode"). The pins are fixed by the
//! PCB, so they are not user-configurable: a wrong pin (e.g. GPIO 6-11, the
//! flash bus) would stop the board from booting.
//!
//! | Signal              | GPIO | Notes                                         |
//! |---------------------|------|-----------------------------------------------|
//! | LED output 1..4     | 12, 14, 27, 26 | SIG1..SIG4, through the 74HCT245    |
//! | Level shifter OE    | 25   | active low, 10k pull-up: off until driven low |
//! | Status LED (WS2812) | 16   | through the level shifter (needs OE low)      |
//! | APA102 clock        | 17   | through the level shifter                     |
//! | I2C SDA / SCL       | 21 / 22 | OLED (and INA226 on newer boards)          |
//! | W5500 SCK/MISO/MOSI | 18 / 19 / 23 | VSPI (SPI3) native pins               |
//! | W5500 CS / INT      | 5 / 4 | W5500 reset is tied to EN                    |
//! | Boot button         | 0    |                                               |
//! | Free on old boards  | 13, 32, 33, 34, 35 | 32/35 are NTCs on newer boards  |

use core::convert::Infallible;

/// GPIO of LED output 1..4 (SIG1..SIG4; `ledout::LedHw`). Also handed to
/// WLED, so it drives the same pins.
pub const LED_PINS: [u8; 4] = [12, 14, 27, 26];

/// The chip's base MAC (efuse). The default device name is derived from it.
pub fn base_mac() -> [u8; 6] {
    let mut m = [0u8; 6];
    m.copy_from_slice(esp_hal::efuse::base_mac_address().as_bytes());
    m
}

/// The Ethernet MAC: base + 3 (Espressif convention; the Wi-Fi AP is base + 1).
pub fn eth_mac() -> [u8; 6] {
    let mut m = base_mac();
    m[5] = m[5].wrapping_add(3);
    m
}

/// The setup Wi-Fi (AP) MAC: base + 1.
pub fn ap_mac() -> [u8; 6] {
    let mut m = base_mac();
    m[5] = m[5].wrapping_add(1);
    m
}

/// SPI clock for the first setup; the start-up probe (`probe_w5500`) then
/// picks the fastest that passes its checks (80 MHz half-duplex here).
pub const W5500_SPI_MHZ: u32 = 40;

/// The W5500 reset line is wired to the ESP32's EN pin, so the MCU cannot
/// drive it. The wiznet driver also resets the chip in software (MR register),
/// which covers a firmware restart.
pub struct NoReset;

impl embedded_hal::digital::ErrorType for NoReset {
    type Error = Infallible;
}

impl embedded_hal::digital::OutputPin for NoReset {
    fn set_low(&mut self) -> Result<(), Infallible> {
        Ok(())
    }

    fn set_high(&mut self) -> Result<(), Infallible> {
        Ok(())
    }
}

/// Counters for the W5500 SPI (cumulative; for measuring the receive path).
/// Background reads that did not end and were cancelled.
pub static SPI_BULK_STUCK: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
pub static SPI_TXNS: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
pub static SPI_BUSY_US: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
/// SPI clock the start-up probe settled on (MHz), and whether reads use the
/// half-duplex path.
pub static SPI_MHZ: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
pub static SPI_HALF: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// Longest W5500 transaction: a full Ethernet frame plus the 3-byte header.
pub const MAX_TXN: usize = 1600;

/// The W5500's SPI bus.
pub type Bus = esp_hal::spi::master::SpiDma<'static, esp_hal::Blocking>;
type BulkXfer = esp_hal::spi::master::SpiDmaTransfer<'static, esp_hal::Blocking, esp_hal::dma::DmaRxBuf>;

/// SPI device for the W5500 that sends each transaction as one transfer.
///
/// The W5500 driver splits every register access into three operations
/// (address, control byte, data); `ExclusiveDevice` turns each into its own
/// DMA transfer with its own setup cost, so a received packet took 24
/// transfers. This copies the operations into one buffer, sends it in one
/// blocking transfer with CS held low, and copies the read parts back.
pub struct W5500Device {
    /// `None` only while a background read runs (the transfer owns it).
    bus: Option<Bus>,
    cs: esp_hal::gpio::Output<'static>,
    buf: &'static mut [u8; MAX_TXN],
    /// Do reads as half-duplex transfers (see [`W5500Read`]); the start-up
    /// probe decides.
    pub half_duplex: bool,
    /// Background RX buffer reads (see `embassy_net_wiznet::BulkRead`): the
    /// DMA buffer while idle, the running transfer, the finished buffer.
    bulk_idle: Option<esp_hal::dma::DmaRxBuf>,
    bulk_xfer: Option<BulkXfer>,
    bulk_done: Option<esp_hal::dma::DmaRxBuf>,
    /// The last background read was cancelled (stuck); its data is invalid.
    bulk_failed: bool,
    /// The driver's receive buffer, until the driver takes it.
    rx_buf: Option<&'static mut [u8; embassy_net_wiznet::BULK]>,
    /// Takes received frames before the network stack (true: handled).
    rx_hook: fn(&[u8]) -> bool,
}

impl W5500Device {
    pub fn new(
        bus: Bus,
        mut cs: esp_hal::gpio::Output<'static>,
        buf: &'static mut [u8; MAX_TXN],
        bulk: esp_hal::dma::DmaRxBuf,
        rx_buf: &'static mut [u8; embassy_net_wiznet::BULK],
        rx_hook: fn(&[u8]) -> bool,
    ) -> Self {
        cs.set_high();
        Self {
            bus: Some(bus),
            cs,
            buf,
            half_duplex: false,
            bulk_idle: Some(bulk),
            bulk_xfer: None,
            bulk_done: None,
            bulk_failed: false,
            rx_buf: Some(rx_buf),
            rx_hook,
        }
    }

    pub fn bus_mut(&mut self) -> &mut Bus {
        self.settle();
        self.bus.as_mut().unwrap_or_else(|| unreachable!("settle() puts the bus back"))
    }

    /// Waits for a background read to end and keeps its data for
    /// `bulk_finish`, so the bus is free for another transfer.
    fn settle(&mut self) {
        if let Some(mut x) = self.bulk_xfer.take() {
            let w = esp_hal::time::Instant::now();
            // A 4 KB read takes ~0.45 ms; one that has not ended after 5 ms
            // is stuck (DMA): cancel it instead of hanging this core until
            // the watchdog, and report the chunk as failed.
            while !x.is_done() {
                if w.elapsed() > esp_hal::time::Duration::from_millis(5) {
                    x.cancel();
                    SPI_BULK_STUCK.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
                    self.bulk_failed = true;
                    break;
                }
            }
            let (bus, buf) = x.wait();
            clear_half_duplex_phases();
            self.cs.set_high();
            self.bus = Some(bus);
            self.bulk_done = Some(buf);
        }
    }
}

impl embedded_hal_async::spi::ErrorType for W5500Device {
    type Error = esp_hal::spi::Error;
}

impl embassy_net_wiznet::BulkRead for W5500Device {
    fn take_rx_buffer(&mut self) -> Option<&'static mut [u8; embassy_net_wiznet::BULK]> {
        self.rx_buf.take()
    }

    fn rx_hook(&mut self, frame: &[u8]) -> bool {
        (self.rx_hook)(frame)
    }

    fn bulk_start(&mut self, ptr: u16, len: usize) -> bool {
        use esp_hal::spi::master::{Address, Command, DataMode};
        self.settle();
        // The driver collects every read it starts (`bulk_finish`); should
        // one ever be left over, drop its data and reuse the buffer rather
        // than refusing every later read (Ethernet RX would stay dead).
        if let Some(buf) = self.bulk_done.take() {
            self.bulk_failed = false;
            self.bulk_idle = Some(buf);
        }
        let (Some(bus), Some(mut buf)) = (self.bus.take(), self.bulk_idle.take()) else {
            return false;
        };
        if len > buf.capacity() {
            self.bus = Some(bus);
            self.bulk_idle = Some(buf);
            return false;
        }
        buf.set_length(len);
        // Socket 0 RX buffer (block 3), read, variable length.
        let header = u32::from(ptr) << 8 | 0b0001_1000;
        self.cs.set_low();
        let started = bus.half_duplex_read_buffer(
            DataMode::SingleTwoDataLines,
            Command::None,
            Address::_24Bit(header, DataMode::SingleTwoDataLines),
            0,
            len,
            buf,
        );
        match started {
            Ok(x) => {
                self.bulk_xfer = Some(x);
                SPI_TXNS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
                true
            }
            Err((_, bus, buf)) => {
                clear_half_duplex_phases();
                self.cs.set_high();
                self.bus = Some(bus);
                self.bulk_idle = Some(buf);
                false
            }
        }
    }

    fn bulk_finish(&mut self, dst: &mut [u8]) -> Result<(), Self::Error> {
        self.settle();
        let Some(buf) = self.bulk_done.take() else {
            return Err(esp_hal::spi::Error::Unsupported);
        };
        if core::mem::take(&mut self.bulk_failed) {
            self.bulk_idle = Some(buf);
            return Err(esp_hal::spi::Error::Unsupported);
        }
        let n = dst.len().min(buf.as_slice().len());
        dst[..n].copy_from_slice(&buf.as_slice()[..n]);
        self.bulk_idle = Some(buf);
        Ok(())
    }

    fn bulk_error(&mut self) -> Self::Error {
        esp_hal::spi::Error::Unsupported
    }
}

/// A bus that can do a W5500 read as one half-duplex transfer: 24 header
/// bits out (address and control byte), then data in. The ESP32's half-duplex
/// mode delays sampling MISO to make up for the GPIO matrix delay (extra
/// dummy clock + MISO delay, as in ESP-IDF); full-duplex transfers get no
/// such compensation, which is why plain transfers read garbage at 40 MHz.
pub trait W5500Read: embedded_hal::spi::SpiBus {
    fn read_after_header(&mut self, header: u32, buf: &mut [u8]) -> Result<(), Self::Error>;
    /// The same for a write: header as the address phase, then the data.
    /// (A full-duplex DMA transfer cost ~60 us even for one register byte.)
    fn write_after_header(&mut self, header: u32, data: &[u8]) -> Result<(), Self::Error>;
}

impl W5500Read for esp_hal::spi::master::SpiDma<'static, esp_hal::Blocking> {
    fn read_after_header(&mut self, header: u32, buf: &mut [u8]) -> Result<(), Self::Error> {
        use esp_hal::spi::master::{Address, Command, DataMode};
        let r = self.half_duplex_read(
            DataMode::SingleTwoDataLines,
            Command::None,
            Address::_24Bit(header, DataMode::SingleTwoDataLines),
            0,
            buf,
        );
        if r.is_ok() {
            fifo::snapshot(&fifo::READ);
        }
        clear_half_duplex_phases();
        r
    }

    fn write_after_header(&mut self, header: u32, data: &[u8]) -> Result<(), Self::Error> {
        use esp_hal::spi::master::{Address, Command, DataMode};
        let r = self.half_duplex_write(
            DataMode::SingleTwoDataLines,
            Command::None,
            Address::_24Bit(header, DataMode::SingleTwoDataLines),
            0,
            data,
        );
        if r.is_ok() {
            fifo::snapshot(&fifo::WRITE);
        }
        clear_half_duplex_phases();
        r
    }
}

/// Short W5500 transfers (registers) straight through the SPI FIFO, without
/// esp-hal: its DMA path costs ~40 us per transfer even for one byte, about
/// six of them per 4 KB read chunk. The settings (phases, dummy cycles, the
/// MISO delay that makes 80 MHz half-duplex reads work) are copied from the
/// registers after a successful esp-hal transfer and replayed. esp-hal
/// arms its DMA links again for each of its own transfers; this clears
/// them (on the ESP32 the links decide between DMA and FIFO).
pub mod fifo {
    use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

    /// user, user1, ctrl2 as esp-hal left them after a half-duplex read/write.
    pub struct Snap([AtomicU32; 3], AtomicBool);
    pub static READ: Snap = Snap([const { AtomicU32::new(0) }; 3], AtomicBool::new(false));
    pub static WRITE: Snap = Snap([const { AtomicU32::new(0) }; 3], AtomicBool::new(false));
    /// Switched on after the start-up self-test; off again after a stuck transfer.
    pub static ON: AtomicBool = AtomicBool::new(false);
    pub static STUCK: AtomicU32 = AtomicU32::new(0);
    /// Bytes per FIFO transfer (the W5500 registers we use are 1-2 bytes).
    pub const MAX: usize = 32;

    macro_rules! regs {
        () => {
            esp_hal::peripherals::SPI3::regs()
        };
    }

    pub fn snapshot(s: &Snap) {
        if s.1.load(Ordering::Relaxed) {
            return;
        }
        let r = regs!();
        s.0[0].store(r.user().read().bits(), Ordering::Relaxed);
        s.0[1].store(r.user1().read().bits(), Ordering::Relaxed);
        s.0[2].store(r.ctrl2().read().bits(), Ordering::Relaxed);
        s.1.store(true, Ordering::Relaxed);
    }

    pub fn ready() -> bool {
        READ.1.load(Ordering::Relaxed) && WRITE.1.load(Ordering::Relaxed)
    }

    /// One transfer: header (address phase), then `len` bytes in (read) or
    /// out (write, data already in the W registers). False if it did not end.
    #[esp_hal::ram]
    fn run(s: &Snap, header: u32, len: usize) -> bool {
        let r = regs!();
        unsafe {
            r.dma_out_link().write(|w| w.bits(0));
            r.dma_in_link().write(|w| w.bits(0));
            r.user().write(|w| w.bits(s.0[0].load(Ordering::Relaxed)));
            r.user1().write(|w| w.bits(s.0[1].load(Ordering::Relaxed)));
            r.ctrl2().write(|w| w.bits(s.0[2].load(Ordering::Relaxed)));
            r.addr().write(|w| w.bits(header << 8));
            let bits = (len as u32) * 8 - 1;
            r.mosi_dlen().write(|w| w.bits(bits));
            r.miso_dlen().write(|w| w.bits(bits));
        }
        r.cmd().modify(|_, w| w.usr().set_bit());
        let mut spins = 0u32;
        while r.cmd().read().usr().bit_is_set() {
            spins += 1;
            if spins > 20_000 {
                // ~100 us+; a 34-byte transfer takes ~4 us. Give up for good.
                ON.store(false, Ordering::Relaxed);
                STUCK.fetch_add(1, Ordering::Relaxed);
                super::clear_half_duplex_phases();
                return false;
            }
        }
        super::clear_half_duplex_phases();
        true
    }

    /// Half-duplex read of up to [`MAX`] bytes.
    #[esp_hal::ram]
    pub fn read(header: u32, buf: &mut [u8]) -> bool {
        if buf.is_empty() || buf.len() > MAX || !run(&READ, header, buf.len()) {
            return false;
        }
        for (k, chunk) in buf.chunks_mut(4).enumerate() {
            let v = regs!().w(k).read().bits().to_le_bytes();
            chunk.copy_from_slice(&v[..chunk.len()]);
        }
        true
    }

    /// Half-duplex write of up to [`MAX`] bytes.
    #[esp_hal::ram]
    pub fn write(header: u32, data: &[u8]) -> bool {
        if data.is_empty() || data.len() > MAX {
            return false;
        }
        for (k, chunk) in data.chunks(4).enumerate() {
            let mut v = [0u8; 4];
            v[..chunk.len()].copy_from_slice(chunk);
            unsafe { regs!().w(k).write(|w| w.bits(u32::from_le_bytes(v))) };
        }
        run(&WRITE, header, data.len())
    }
}

/// esp-hal 1.2.2 bug: switching back to full duplex clears the dummy phase
/// but not the address/command phases, so every later full-duplex transfer
/// would send a stale 24-bit address first.
fn clear_half_duplex_phases() {
    esp_hal::peripherals::SPI3::regs().user().modify(|_, w| {
        w.usr_addr().clear_bit();
        w.usr_command().clear_bit()
    });
}

/// The 24-bit header of a W5500 read: the operations start with writes of
/// exactly 3 bytes in total whose control byte has the write bit clear,
/// followed by one read. Returns the header and the index of the read.
fn w5500_read_header(ops: &[embedded_hal_async::spi::Operation<'_, u8>]) -> Option<(u32, usize)> {
    use embedded_hal_async::spi::Operation;
    let mut header = 0u32;
    let mut n = 0;
    for (i, op) in ops.iter().enumerate() {
        match op {
            Operation::Write(w) => {
                for &b in w.iter() {
                    header = header << 8 | u32::from(b);
                    n += 1;
                }
            }
            Operation::Read(_) | Operation::TransferInPlace(_)
                if n == 3 && header & 0b100 == 0 && i + 1 == ops.len() =>
            {
                return Some((header, i));
            }
            _ => return None,
        }
    }
    None
}

/// A W5500 write: only writes, a 3-byte header whose control byte has the
/// write bit set, and at least one data byte. Gathers it into `buf` and
/// returns the length.
fn w5500_write_ops(ops: &[embedded_hal_async::spi::Operation<'_, u8>], buf: &mut [u8]) -> Option<usize> {
    use embedded_hal_async::spi::Operation;
    let mut n = 0;
    for op in ops.iter() {
        let Operation::Write(w) = op else { return None };
        buf.get_mut(n..n + w.len())?.copy_from_slice(w);
        n += w.len();
    }
    (n > 3 && buf[2] & 0b100 != 0).then_some(n)
}

impl embedded_hal_async::spi::SpiDevice for W5500Device {
    async fn transaction(
        &mut self,
        ops: &mut [embedded_hal_async::spi::Operation<'_, u8>],
    ) -> Result<(), Self::Error> {
        use embedded_hal_async::spi::Operation;
        // A background read must end before anything else uses the bus.
        self.settle();
        let Some(bus) = self.bus.as_mut() else {
            unreachable!("settle() puts the bus back")
        };
        let start = esp_hal::time::Instant::now();
        if let Some((header, i)) = w5500_read_header(ops).filter(|_| self.half_duplex) {
            let buf: &mut [u8] = match &mut ops[i] {
                Operation::Read(r) => r,
                Operation::TransferInPlace(d) => d,
                _ => unreachable!("checked by w5500_read_header"),
            };
            let len = buf.len();
            self.cs.set_low();
            let fast = len <= fifo::MAX && fifo::ON.load(core::sync::atomic::Ordering::Relaxed) && fifo::read(header, buf);
            let r = if fast { Ok(()) } else { bus.read_after_header(header, buf) };
            self.cs.set_high();
            let us = start.elapsed().as_micros() as u32;
            SPI_TXNS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            SPI_BUSY_US.fetch_add(us, core::sync::atomic::Ordering::Relaxed);
            return r;
        }
        if self.half_duplex
            && let Some(len) = w5500_write_ops(ops, self.buf)
        {
            let header = u32::from(self.buf[0]) << 16 | u32::from(self.buf[1]) << 8 | u32::from(self.buf[2]);
            self.cs.set_low();
            let fast = len - 3 <= fifo::MAX
                && fifo::ON.load(core::sync::atomic::Ordering::Relaxed)
                && fifo::write(header, &self.buf[3..len]);
            let r = if fast { Ok(()) } else { bus.write_after_header(header, &self.buf[3..len]) };
            self.cs.set_high();
            let us = start.elapsed().as_micros() as u32;
            SPI_TXNS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            SPI_BUSY_US.fetch_add(us, core::sync::atomic::Ordering::Relaxed);
            return r;
        }
        // Gather everything that goes out on MOSI.
        let mut len = 0;
        for op in ops.iter() {
            let (n, src): (usize, Option<&[u8]>) = match op {
                Operation::Write(w) => (w.len(), Some(w)),
                Operation::Transfer(r, w) => {
                    let n = r.len().max(w.len());
                    self.buf[len..len + n].fill(0);
                    self.buf[len..len + w.len()].copy_from_slice(w);
                    (n, None)
                }
                Operation::TransferInPlace(d) => (d.len(), Some(d)),
                Operation::Read(r) => {
                    self.buf[len..len + r.len()].fill(0);
                    (r.len(), None)
                }
                Operation::DelayNs(_) => (0, None),
            };
            if let Some(src) = src {
                self.buf[len..len + n].copy_from_slice(src);
            }
            len += n;
        }
        self.cs.set_low();
        let r = embedded_hal::spi::SpiBus::transfer_in_place(bus, &mut self.buf[..len])
            .and_then(|_| embedded_hal::spi::SpiBus::flush(bus));
        self.cs.set_high();
        // Hand back what came in on MISO.
        let mut at = 0;
        for op in ops.iter_mut() {
            match op {
                Operation::Write(w) => at += w.len(),
                Operation::Transfer(r, w) => {
                    let n = r.len().max(w.len());
                    r.copy_from_slice(&self.buf[at..at + r.len()]);
                    at += n;
                }
                Operation::TransferInPlace(d) => {
                    d.copy_from_slice(&self.buf[at..at + d.len()]);
                    at += d.len();
                }
                Operation::Read(r) => {
                    r.copy_from_slice(&self.buf[at..at + r.len()]);
                    at += r.len();
                }
                Operation::DelayNs(_) => {}
            }
        }
        let us = start.elapsed().as_micros() as u32;
        SPI_TXNS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        SPI_BUSY_US.fetch_add(us, core::sync::atomic::Ordering::Relaxed);
        r
    }
}
