//! W5500 start-up: the SPI clock probe, link checks, the FIFO self-test and the DMA buffers of the SPI driver.

use super::*;

/// Reads the W5500 version register (0x0039, common block), first at the
/// normal SPI clock, then slower, logging what comes back. Keeps the first
/// clock that works. MISO idles high, so a chip that does not answer reads
/// 0xFF; with nothing attached it may read 0x00.
pub(crate) async fn probe_w5500(spi: &mut W5500Spi) -> bool {
    // Fastest first. Half-duplex reads compensate the MISO delay that makes
    // plain reads fail at 40 MHz on this board; each combination must pass
    // all checks, and the last ones are what always worked before.
    const TRIES: [(u32, bool); 9] = [
        (80, true),
        (40, true),
        (40, false),
        (26, true),
        (26, false),
        (20, false),
        (10, false),
        (4, false),
        (1, false),
    ];
    for (mhz, half) in TRIES {
        let cfg = SpiConfig::default().with_frequency(Rate::from_mhz(mhz)).with_mode(Mode::_0);
        if let Err(e) = spi.bus_mut().apply_config(&cfg) {
            warn!("W5500 probe: cannot set {} MHz: {:?}", mhz, e);
            continue;
        }
        spi.half_duplex = half;
        match w5500_check(spi).await {
            Ok(()) => {
                if half && mhz == 80 {
                    fifo_self_test(spi).await;
                }
                info!("W5500 SPI at {} MHz{}", mhz, if half { ", half-duplex reads" } else { "" });
                board::SPI_MHZ.store(mhz, Ordering::Relaxed);
                board::SPI_HALF.store(half, Ordering::Relaxed);
                return true;
            }
            Err(why) => info!("W5500 probe at {} MHz{}: {}", mhz, if half { " half-duplex" } else { "" }, why),
        }
    }
    spi.half_duplex = false;
    warn!("No W5500 answered on SPI (SCK 18, MOSI 23, MISO 19, CS 5) at any clock");
    false
}

/// Switches the FIFO path for short W5500 transfers on if it reads and
/// writes correctly: 100 version reads and 100 two-byte write/read-backs
/// through it (`board::fifo`). Stays off otherwise.
pub(crate) async fn fifo_self_test(spi: &mut W5500Spi) {
    use embedded_hal_async::spi::{Operation, SpiDevice};
    if !board::fifo::ready() {
        warn!("W5500 FIFO path: no esp-hal settings to copy; staying off");
        return;
    }
    board::fifo::ON.store(true, Ordering::Relaxed);
    const VERSIONR: [u8; 3] = [0x00, 0x39, 0x00];
    const WR: [u8; 3] = [0x00, 0x00, 0b0001_0100];
    const RD: [u8; 3] = [0x00, 0x00, 0b0001_0000];
    let mut ok = true;
    for k in 0..100u16 {
        let mut v = [0u8; 1];
        let r = spi.transaction(&mut [Operation::Write(&VERSIONR), Operation::Read(&mut v)]).await;
        let p = (k.wrapping_mul(40503) ^ 0x5AA5).to_be_bytes();
        let w = spi.transaction(&mut [Operation::Write(&WR), Operation::Write(&p)]).await;
        let mut b = [0u8; 2];
        let rb = spi.transaction(&mut [Operation::Write(&RD), Operation::Read(&mut b)]).await;
        if r.is_err() || v[0] != 0x04 || w.is_err() || rb.is_err() || b != p {
            ok = false;
            break;
        }
    }
    let ok = ok && board::fifo::ON.load(Ordering::Relaxed);
    board::fifo::ON.store(ok, Ordering::Relaxed);
    if ok {
        info!("W5500 FIFO path for short transfers: on");
    } else {
        warn!("W5500 FIFO path for short transfers: self-test failed; staying off");
    }
}

/// Checks the W5500 link at the current setting: 100 version reads, then
/// 100 times a 1 KB pattern written to socket 0's TX buffer and read back
/// (the driver resets the chip afterwards).
pub(crate) async fn w5500_check(spi: &mut W5500Spi) -> Result<(), &'static str> {
    use embedded_hal_async::spi::{Operation, SpiDevice};
    const VERSIONR: [u8; 3] = [0x00, 0x39, 0x00]; // address, then BSB=0 read VDM
    for _ in 0..100 {
        let mut v = [0u8; 1];
        let r = spi.transaction(&mut [Operation::Write(&VERSIONR), Operation::Read(&mut v)]).await;
        if r.is_err() || v[0] != 0x04 {
            return Err("version read wrong");
        }
    }
    const WR: [u8; 3] = [0x00, 0x00, 0b0001_0100]; // addr 0, S0 TX buffer, write
    const RD: [u8; 3] = [0x00, 0x00, 0b0001_0000]; // addr 0, S0 TX buffer, read
    let mut pat = [0u8; 1024];
    let mut back = [0u8; 1024];
    for k in 0..100u32 {
        for (j, b) in pat.iter_mut().enumerate() {
            *b = (j as u32).wrapping_mul(31).wrapping_add(k * 17) as u8 ^ 0x5A;
        }
        let w = spi.transaction(&mut [Operation::Write(&WR), Operation::Write(&pat)]).await;
        let r = spi.transaction(&mut [Operation::Write(&RD), Operation::Read(&mut back)]).await;
        if w.is_err() || r.is_err() || back != pat {
            return Err("1 KB read-back wrong");
        }
    }
    // Small writes (registers): 100 two-byte patterns.
    for k in 0..100u16 {
        let p = (k.wrapping_mul(7919) ^ 0xA55A).to_be_bytes();
        let w = spi.transaction(&mut [Operation::Write(&WR), Operation::Write(&p)]).await;
        let mut b = [0u8; 2];
        let r = spi.transaction(&mut [Operation::Write(&RD), Operation::Read(&mut b)]).await;
        if w.is_err() || r.is_err() || b != p {
            return Err("small write read-back wrong");
        }
    }
    Ok(())
}

/// The SPI DMA receive buffer for the W5500: 4 KB (one bulk read of the
/// driver), in the reclaimed RAM (internal SRAM, DMA-capable).
pub(crate) fn w5500_dma_bulk() -> esp_hal::dma::DmaRxBuf {
    use esp_hal::dma::{DmaRxBuf, aligned::InternalMemory};
    const LEN: usize = embassy_net_wiznet::CHUNK;
    let desc = esp_hal::dma_descriptors_impl!(LEN, esp_hal::dma::BurstConfig::DEFAULT.max_compatible_chunk_size());
    let buf = esp32_pixelcast::reclaimed_zeroed!(InternalMemory<[u8; LEN]>).get_mut().unsize();
    DmaRxBuf::new(desc, buf).unwrap_or_else(|_| unreachable!("the descriptors cover the buffer"))
}

/// The SPI driver's own DMA receive buffer: registers and the start-up
/// check's 1 KB read-back (bulk reads use `w5500_dma_bulk`). Reclaimed RAM.
pub(crate) fn w5500_dma_small() -> esp_hal::dma::DmaRxBuf {
    use esp_hal::dma::{DmaRxBuf, aligned::InternalMemory};
    const LEN: usize = 1024;
    let desc = esp_hal::dma_descriptors_impl!(LEN, esp_hal::dma::BurstConfig::DEFAULT.max_compatible_chunk_size());
    let buf = esp32_pixelcast::reclaimed_zeroed!(InternalMemory<[u8; LEN]>).get_mut().unsize();
    DmaRxBuf::new(desc, buf).unwrap_or_else(|_| unreachable!("the descriptors cover the buffer"))
}

/// The driver's receive buffer. In normal RAM (taken from the main stack's
/// unused part, see `stackwatch`): the reclaimed RAM goes to the LED pool.
pub(crate) fn w5500_rx_buf() -> &'static mut [u8; embassy_net_wiznet::BULK] {
    static BUF: static_cell::ConstStaticCell<[u8; embassy_net_wiznet::BULK]> =
        static_cell::ConstStaticCell::new([0; embassy_net_wiznet::BULK]);
    BUF.take()
}
