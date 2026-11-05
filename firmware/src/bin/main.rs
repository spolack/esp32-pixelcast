#![no_std]
#![no_main]
#![deny(
    clippy::mem_forget,
    reason = "mem::forget is generally not safe to do with esp_hal types, especially those \
    holding buffers for the duration of a data transfer."
)]
#![deny(clippy::large_stack_frames)]

use core::{
    net::{Ipv4Addr, SocketAddr, SocketAddrV4},
    sync::atomic::{AtomicBool, AtomicU32, Ordering},
};

use esp32_pixelcast::merge as artnet_merge;
use esp32_pixelcast::{
    artnet, board, config, core1, led, sacn, testpattern,
    ota::{self, SECTOR},
    settings::{self, EthMode, Settings},
    status,
    ledout,
    web::{self, FlashCtx, SharedFlash, WebCtx},
};
use embassy_executor::Spawner;
use embassy_futures::select::{Either, select};
use embassy_net::{
    ConfigV4, Ipv4Cidr, Stack, StackResources, StaticConfigV4,
    udp::{PacketMetadata, UdpSocket},
};
use embassy_net_wiznet::{State, chip::W5500};
use embassy_sync::once_lock::OnceLock;
use embassy_time::{Duration, Instant, Timer, with_timeout};
use esp_backtrace as _;
use esp_bootloader_esp_idf::partitions::FlashStorage;
use esp_hal::{
    clock::CpuClock,
    gpio::{Input, InputConfig, Level, Output, OutputConfig, Pull},
    peripherals::WIFI,
    rng::Rng,
    system::Stack as CoreStack,
    spi::{
        Mode,
        master::{Config as SpiConfig, Spi},
    },
    time::Rate,
    timer::timg::{MwdtStage, TimerGroup, Wdt},
};
use esp_rtos::embassy::Executor;
use esp_radio::wifi::{
    AuthenticationMethodConfig, Config as WifiConfig, ControllerConfig, Interface as WifiInterface,
    WifiController, ap::AccessPointConfig,
};
use log::{error, info, warn};
use static_cell::{ConstStaticCell, StaticCell};

extern crate alloc;

// This creates a default app-descriptor required by the esp-idf bootloader.
// Version with the commit and the build time: the update page shows them
// per slot (see build.rs).
esp_bootloader_esp_idf::esp_app_desc!(
    env!("PXC_VERSION"),
    env!("CARGO_PKG_NAME"),
    env!("PXC_BUILD_TIME"),
    env!("PXC_BUILD_DATE"),
    esp_bootloader_esp_idf::ESP_IDF_COMPATIBLE_VERSION,
    esp_bootloader_esp_idf::MMU_PAGE_SIZE,
    0,
    u16::MAX,
    esp_bootloader_esp_idf::SECURE_VERSION
);

type W5500Spi = board::W5500Device;
type WiznetRunner = embassy_net_wiznet::Runner<'static, W5500, W5500Spi, Input<'static>, board::NoReset>;
type EthRunner = embassy_net::Runner<'static, embassy_net_wiznet::Device<'static>>;
type ApRunner = embassy_net::Runner<'static, WifiInterface>;

// Parts of the firmware, in src/bin/main/ (not src/bin/, where Cargo would
// take each file for a program of its own).
#[path = "main/ap.rs"]
mod ap;
#[path = "main/button.rs"]
mod button;
#[path = "main/eth.rs"]
mod eth;
#[path = "main/indicators.rs"]
mod indicators;
#[path = "main/rx.rs"]
mod rx;
#[path = "main/w5500.rs"]
mod w5500;

use ap::*;
use button::*;
use eth::*;
use indicators::*;
use rx::*;
use w5500::*;

/// Packets the W5500 driver queues between the chip and the network stack.
/// The W5500 itself buffers 16 KB of received frames, so a short queue is
/// enough; we send little.
const W5500_RX_QUEUE: usize = 4;
const W5500_TX_QUEUE: usize = 2;

/// A panic restarts the board instead of halting it, so a broken update is
/// rolled back on the next boot. The message goes to the crash log (shown on
/// the status page after the restart) and to the serial port.
#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    esp32_pixelcast::crashlog::panic(info);
    esp32_pixelcast::flashinfo::note_crash();
    esp_println::println!("PANIC: {}", info);
    esp_hal::system::software_reset()
}

/// esp-backtrace's halt (CPU exceptions it reports): restart as well.
#[unsafe(no_mangle)]
extern "Rust" fn custom_halt() -> ! {
    esp32_pixelcast::flashinfo::note_crash();
    esp_hal::system::software_reset()
}

#[allow(
    clippy::large_stack_frames,
    reason = "it's not unusual to allocate larger buffers etc. in main"
)]
#[esp_rtos::main]
async fn main(spawner: Spawner) -> ! {
    esp32_pixelcast::stackwatch::paint();
    esp_println::logger::init_logger_from_env();
    esp32_pixelcast::crashlog::boot();

    let config = esp_hal::Config::default().with_cpu_clock(CpuClock::max());
    let peripherals = esp_hal::init(config);

    // The rest of the reclaimed RAM holds the LED memory pool
    // (`ledout::POOL_BYTES`), the second core's stack (12 KB) and the W5500
    // buffers. The heap: 12 KB here plus 8 KB in normal RAM. Starting the setup Wi-Fi
    // adds 40 KB of the LED memory (see `ledout::shrink_for_wifi`); the
    // Wi-Fi driver takes about 47 KB.
    esp_alloc::heap_allocator!(#[esp_hal::ram(reclaimed)] size: 12288);
    esp_alloc::heap_allocator!(size: 8192);

    let timg0 = TimerGroup::new(peripherals.TIMG0);
    esp_rtos::start(timg0.timer0, peripherals.FROM_CPU_INTR0);

    // ── Level shifter ────────────────────────────────────────────────────────
    // GPIO25 is the shifter's active-low output enable (10k pull-up on the
    // board): driven low for good, as WLED does. Signals behind the shifter
    // that I2S does not own are driven low too.
    let out = OutputConfig::default();
    core::mem::forget(Output::new(peripherals.GPIO25, Level::Low, out));
    let status_led = Output::new(peripherals.GPIO16, Level::Low, out);
    core::mem::forget(Output::new(peripherals.GPIO17, Level::Low, out));

    // ── Firmware slot check ──────────────────────────────────────────────────
    // Runs before anything that could fail, so a broken update gets rolled
    // back even if it crashes later during start-up.
    // SAFETY: the second core (LED output) is parked by `core1::pause` around
    // every flash write; before it starts there is nothing to park.
    let mut flash = unsafe { FlashStorage::new(peripherals.FLASH).multicore_ignore() };
    let boot = ota::boot_check(&mut flash, &core1::before_start());
    esp32_pixelcast::dualboot::detect(&mut flash);
    let layout = ota::table_state(&mut flash);
    ota::read_next_slot(&mut flash);
    web::BOOTLOADER_OURS.store(ota::bootloader_current(&mut flash), Ordering::Relaxed);
    let installed = ota::bootloader_level(&mut flash);
    web::BOOTLOADER_COMPATIBLE.store(
        installed.is_some() && installed >= ota::bundled_bootloader_level(),
        Ordering::Relaxed,
    );
    let slots = ota::slots(&mut flash);
    let in_place_note = ota::in_place_note(&mut flash);
    esp32_pixelcast::flashinfo::probe();
    settings::set_current(settings::load(&mut flash));
    info!(
        "{} {} running from {}{}",
        env!("CARGO_PKG_NAME"),
        env!("PXC_VERSION"),
        boot.slot,
        if boot.trial { " (trial boot)" } else { "" }
    );
    static FLASH: StaticCell<SharedFlash> = StaticCell::new();
    let flash: &'static SharedFlash = FLASH.init(SharedFlash::new(FlashCtx {
        flash,
        buf: [0; SECTOR],
    }));

    // A hang restarts the board too (and so also triggers a rollback).
    let timg1 = TimerGroup::new(peripherals.TIMG1);
    let mut wdt = timg1.wdt;
    wdt.set_timeout(MwdtStage::Stage0, esp_hal::time::Duration::from_secs(10));
    wdt.enable();
    // Optional QIO 80 MHz flash: after the trial check (a hang still rolls
    // back), with the watchdog armed, before the second core starts.
    esp32_pixelcast::flashinfo::start(settings::with(|s| s.fast_flash), &esp32_pixelcast::web::flash_check_data());
    spawner.spawn(watchdog_task(wdt).unwrap());

    if config::DEFAULT_PASSWORD && settings::with(|s| s.password.is_empty()) {
        warn!("The default password is in use. Set one on the settings page.");
    }
    let base_mac = board::base_mac();
    static WEB: StaticCell<WebCtx> = StaticCell::new();
    let web_ctx: &'static WebCtx = WEB.init(WebCtx {
        flash,
        boot,
        mac: base_mac,
        layout,
        slots,
        in_place_note,
    });

    // ── W5500 on VSPI ────────────────────────────────────────────────────────
    // One Ethernet frame is at most 1514 bytes plus a 3-byte SPI header.
    let spi = Spi::new(
        peripherals.SPI3,
        SpiConfig::default()
            .with_frequency(Rate::from_mhz(board::W5500_SPI_MHZ))
            .with_mode(Mode::_0),
    )
    .expect("SPI config")
    .with_sck(peripherals.GPIO18)
    .with_mosi(peripherals.GPIO23)
    .with_miso(peripherals.GPIO19)
    // DMA for every transfer: ~38 us to set up, but a received frame moves
    // at full SPI speed (the CPU-fed FIFO took ~325 us per frame). With the
    // batched receive (patches/embassy-net-wiznet) a packet needs only 2-3
    // transfers. Do not use min_async_transfer_size: mixing FIFO and DMA
    // that way made the W5500 unreadable.
    .with_dma(peripherals.DMA_SPI3)
    .with_buffers(w5500_dma_small(), esp_hal::dma_tx_buffer!(2048).unwrap());
    let cs = Output::new(peripherals.GPIO5, Level::High, out);
    static SPI_BUF: ConstStaticCell<[u8; board::MAX_TXN]> = ConstStaticCell::new([0; board::MAX_TXN]);
    for (a, b) in ETH_MAC.iter().zip(board::eth_mac()) {
        a.store(b, Ordering::Relaxed);
    }
    let mut spi_dev = board::W5500Device::new(spi, cs, SPI_BUF.take(), w5500_dma_bulk(), w5500_rx_buf(), eth_fast_path);
    let w5500_found = probe_w5500(&mut spi_dev).await;
    let int = Input::new(peripherals.GPIO4, InputConfig::default().with_pull(Pull::Up));

    let mac = board::eth_mac();

    // The W5500 module is plugged into the carrier and may be missing. Then
    // the board runs on the setup Wi-Fi alone instead of failing to start.
    static WIZNET_STATE: StaticCell<State<W5500_RX_QUEUE, W5500_TX_QUEUE>> = StaticCell::new();
    let wiznet = embassy_net_wiznet::new::<W5500_RX_QUEUE, W5500_TX_QUEUE, W5500, _, _, _>(
        mac,
        WIZNET_STATE.init(State::new()),
        spi_dev,
        int,
        board::NoReset,
    );
    let eth: Option<Stack<'static>> = if !w5500_found {
        None
    } else {
        match with_timeout(Duration::from_secs(2), wiznet).await {
            Ok(Ok((device, wiznet_runner))) => {
                spawner.spawn(wiznet_task(wiznet_runner).unwrap());
                // Sockets: DHCP client, Art-Net, web servers, no-RST, mDNS, the
                // DHCP server probe. Exactly full: smoltcp panics when one more
                // socket is added.
                static ETH_RESOURCES: StaticCell<StackResources<{ 5 + web::SOCKETS }>> =
                    StaticCell::new();
                let rng = Rng::new();
                let seed = (u64::from(rng.random()) << 32) | u64::from(rng.random());
                let (eth, eth_runner) = embassy_net::new(
                    device,
                    embassy_net::Config::dhcpv4(Default::default()),
                    ETH_RESOURCES.init(StackResources::new()),
                    seed,
                );
                spawner.spawn(eth_net_task(eth_runner).unwrap());
                spawner.spawn(no_rst_task(eth).unwrap());
                info!("W5500 up, MAC {:02x?}", mac);
                status::update(|st| st.eth_present = true);
                Some(eth)
            }
            Ok(Err(e)) => {
                warn!("No W5500 found ({:?}); running without Ethernet", e);
                None
            }
            Err(_) => {
                warn!("W5500 did not answer; running without Ethernet");
                None
            }
        }
    };

    // One Art-Net receiver, on whichever network is active (see `active`).
    // A full 16-universe frame is ~9 KB arriving back to back; the buffer
    // holds two frames plus slack.
    static ARTNET_RX_META: ConstStaticCell<[PacketMetadata; 64]> =
        ConstStaticCell::new([PacketMetadata::EMPTY; 64]);
    // ~33 Art-Net/sACN packets of slack for bursts.
    static ARTNET_RX_BUF: ConstStaticCell<[u8; 22 * 1024]> = ConstStaticCell::new([0; 22 * 1024]);
    spawner.spawn(artnet_task(eth, ARTNET_RX_META.take(), ARTNET_RX_BUF.take()).unwrap());
    spawner.spawn(test_pattern_task().unwrap());
    spawner.spawn(mdns_task(eth).unwrap());
    spawner.spawn(settings_saver_task(flash).unwrap());
    spawner.spawn(status_led_task(status_led).unwrap());
    // The microphone (waits until audio is switched on in the settings).
    spawner.spawn(
        audio_task(esp32_pixelcast::audio::Mic {
            i2s: peripherals.I2S0,
            dma: peripherals.DMA_I2S0,
            mclk: peripherals.GPIO3,
            // settings::MIC_PINS
            pins: [
                Some(peripherals.GPIO2.into()),
                Some(peripherals.GPIO13.into()),
                Some(peripherals.GPIO15.into()),
                Some(peripherals.GPIO32.into()),
                Some(peripherals.GPIO33.into()),
                Some(peripherals.GPIO34.into()),
                Some(peripherals.GPIO35.into()),
                Some(peripherals.GPIO36.into()),
                Some(peripherals.GPIO39.into()),
            ],
        })
        .unwrap(),
    );
    // Optional OLED on the I2C header (GPIO 21 SDA, 22 SCL).
    match esp_hal::i2c::master::I2c::new(
        peripherals.I2C0,
        esp_hal::i2c::master::Config::default().with_frequency(Rate::from_khz(400)),
    ) {
        Ok(i2c) => {
            let i2c = i2c.with_sda(peripherals.GPIO21).with_scl(peripherals.GPIO22).into_async();
            spawner.spawn(oled_task(i2c).unwrap());
        }
        Err(e) => warn!("oled: I2C setup failed: {:?}", e),
    }

    if let Some(eth) = eth {
        static WEB_RX: ConstStaticCell<[[u8; web::RX_LEN]; web::SOCKETS]> =
            ConstStaticCell::new([[0; web::RX_LEN]; web::SOCKETS]);
        static WEB_TX: ConstStaticCell<[[u8; web::TX_LEN]; web::SOCKETS]> =
            ConstStaticCell::new([[0; web::TX_LEN]; web::SOCKETS]);
        for (rx, tx) in WEB_RX.take().iter_mut().zip(WEB_TX.take().iter_mut()) {
            spawner.spawn(web_task(eth, rx, tx, web_ctx, None).unwrap());
        }
    }

    // ── LED output ───────────────────────────────────────────────────────────
    // Two 16 KB DMA rings in 2 KB chunks (~5 ms of slack each at 3.3 MHz):
    // one is sent while the next frame is pre-filled into the other.
    let rings = [esp_hal::dma_tx_stream_buffer!(16384, 2048), esp_hal::dma_tx_stream_buffer!(16384, 2048)];
    // The LED task runs alone on the second core (APP CPU) with its own
    // executor: the encoder can top up the ring at any time, and its work
    // never delays the network tasks on the first core (whose W5500 SPI
    // transfers are blocking).
    // The second core's stack lives in the reclaimed RAM (with the heap) to
    // leave room for the main stack.
    let led_stack = esp32_pixelcast::reclaimed_zeroed!(CoreStack<12288>);
    let led_hw = ledout::LedHw {
        i2s: peripherals.I2S1,
        dma: peripherals.DMA_I2S1,
        // Outputs 1..4 as the board's original firmware numbers them.
        sig1: peripherals.GPIO12,
        sig2: peripherals.GPIO14,
        sig3: peripherals.GPIO27,
        sig4: peripherals.GPIO26,
    };
    // A restart by other firmware (WLED's esp_restart) leaves the second
    // core clocked; esp-hal then takes it for running and refuses to start
    // it (CoreAlreadyRunning panic, one lost start after every return from
    // WLED). Hold it in reset with its clock off; the start below runs the
    // full sequence (clock on, unstall, reset pulse).
    // SAFETY: DPORT APPCPU_CTRL_A (bit 0 resetting) and APPCPU_CTRL_B (bit 0
    // clock gate) of the ESP32; nothing runs on the second core yet.
    unsafe {
        const APPCPU_CTRL_A: *mut u32 = 0x3FF0_002C as *mut u32;
        const APPCPU_CTRL_B: *mut u32 = 0x3FF0_0030 as *mut u32;
        APPCPU_CTRL_A.write_volatile(APPCPU_CTRL_A.read_volatile() | 1);
        APPCPU_CTRL_B.write_volatile(APPCPU_CTRL_B.read_volatile() & !1);
    }
    esp_rtos::start_second_core(peripherals.CPU_CTRL, peripherals.FROM_CPU_INTR1, led_stack, move || {
        static LED_EXECUTOR: StaticCell<Executor> = StaticCell::new();
        LED_EXECUTOR.init(Executor::new()).run(|spawner| {
            spawner.spawn(led_task(led_hw, rings).unwrap());
            spawner.spawn(core1_pause_task().unwrap());
        })
    });
    core1::mark_started();

    // ── Setup Wi-Fi ──────────────────────────────────────────────────────────
    let button = Input::new(peripherals.GPIO0, InputConfig::default().with_pull(Pull::Up));
    spawner.spawn(button_task(button, flash, web_ctx).unwrap());
    spawner.spawn(ap_task(spawner, peripherals.WIFI, eth, web_ctx).unwrap());

    if boot.trial {
        spawner.spawn(confirm_task(flash).unwrap());
    } else {
        web::CONFIRMED.store(true, Ordering::Relaxed);
    }

    let Some(eth) = eth else {
        loop {
            Timer::after(Duration::from_secs(3600)).await;
        }
    };
    spawner.spawn(eth_link_task(eth).unwrap());
    eth_addressing(eth, mac).await
}

#[embassy_executor::task]
async fn audio_task(mic: esp32_pixelcast::audio::Mic) -> ! {
    esp32_pixelcast::audio::run(mic).await
}

#[embassy_executor::task]
async fn led_task(hw: ledout::LedHw, rings: [esp_hal::dma::DmaTxStreamBuf; 2]) -> ! {
    ledout::run(hw, rings).await
}

#[embassy_executor::task]
async fn core1_pause_task() -> ! {
    core1::pause_task().await
}

#[embassy_executor::task]
async fn wiznet_task(runner: WiznetRunner) -> ! {
    runner.run().await
}

#[embassy_executor::task]
async fn eth_net_task(mut runner: EthRunner) -> ! {
    runner.run().await
}

#[embassy_executor::task]
async fn watchdog_task(mut wdt: Wdt<esp_hal::peripherals::TIMG1<'static>>) -> ! {
    // Also watches the second core: the LED task bumps a counter at least
    // every 5 ms. If it stops for 6 s (three checks), stop feeding and let
    // the watchdog (10 s) restart the board.
    let mut last = ledout::heartbeat();
    let mut stalled = 0u32;
    loop {
        let now = ledout::heartbeat();
        stalled = if now == last { stalled + 1 } else { 0 };
        last = now;
        esp32_pixelcast::crashlog::tick(Instant::now().as_secs() as u32);
        status::HEAP_MIN.fetch_min(esp_alloc::HEAP.free() as u32, Ordering::Relaxed);
        if embassy_net_wiznet::CHIP_RESET_SEEN.load(Ordering::Relaxed) {
            // Simplest complete re-initialisation of the chip and the stack.
            error!("watchdog: the Ethernet chip lost its setup (power glitch?); restarting");
            esp32_pixelcast::crashlog::planned("The Ethernet chip lost its setup (power glitch?)");
            esp_hal::system::software_reset();
        }
        if stalled < 3 {
            wdt.feed();
        } else if stalled == 3 {
            error!("watchdog: the LED core stopped; letting the watchdog restart the board");
            esp32_pixelcast::crashlog::watchdog("The LED core stopped responding");
        }
        Timer::after(Duration::from_secs(2)).await;
    }
}

/// Keeps a freshly updated image only once someone confirms it
/// (`POST /confirm`, accepted after `CONFIRM_AFTER_MS`). Until then a
/// restart rolls back to the previous image; without a confirm by
/// `TRIAL_DEADLINE_MS` this restarts on purpose to roll back.
#[embassy_executor::task]
async fn confirm_task(flash: &'static SharedFlash) {
    // Only an explicit confirm keeps a new image (working traffic is not
    // enough: an image can answer and still be broken under load).
    while !web::CONFIRM_REQUEST.load(Ordering::Relaxed) {
        if Instant::now().as_millis() >= config::TRIAL_DEADLINE_MS {
            error!("ota: the new image was not confirmed in time; restarting to roll back");
            esp32_pixelcast::crashlog::planned("Update not confirmed within 15 min, rolled back");
            Timer::after(Duration::from_millis(200)).await;
            esp_hal::system::software_reset();
        }
        Timer::after(Duration::from_millis(200)).await;
    }
    let mut guard = flash.lock().await;
    // An update installed meanwhile is about to start; leave its record be.
    if web::RESTART_PENDING.load(Ordering::Relaxed) {
        return;
    }
    // A transient flash error must not leave a working image unconfirmed
    // (it would roll back on the next restart), so retry a few times.
    for attempt in 1..=5 {
        let confirmed = {
            let paused = core1::pause().await;
            ota::confirm(&mut guard.flash, &paused)
        };
        match confirmed {
            Ok(()) => {
                web::CONFIRMED.store(true, Ordering::Relaxed);
                return;
            }
            Err(e) => error!("ota: confirming the image failed (try {}): {:?}", attempt, e),
        }
        drop(guard);
        Timer::after(Duration::from_secs(10)).await;
        guard = flash.lock().await;
        if web::RESTART_PENDING.load(Ordering::Relaxed) {
            return;
        }
    }
    // Still on trial: the web pages refuse restarts and updates in this
    // state, so the board would be stuck. Roll back to the previous image.
    error!("ota: the image could not be confirmed; restarting to roll back");
    esp32_pixelcast::crashlog::planned("Update could not be confirmed, rolling back");
    drop(guard);
    Timer::after(Duration::from_millis(200)).await;
    esp_hal::system::software_reset();
}

/// One web server per network, so the setup Wi-Fi works even while the
/// cable is plugged in.
/// One task per listening socket: `web::SOCKETS` per network.
#[embassy_executor::task(pool_size = 2 * web::SOCKETS)]
async fn web_task(
    stack: Stack<'static>,
    rx: &'static mut [u8],
    tx: &'static mut [u8],
    ctx: &'static WebCtx,
    portal_host: Option<&'static str>,
) -> ! {
    web::serve(stack, rx, tx, ctx, portal_host).await
}

/// Saves the settings to flash after network changes (ArtAddress), once no
/// further change came for a second, and at most once a minute: ArtAddress
/// needs no login, so a host repeating it must not wear out the flash (or
/// pause the LED core for every erase).
#[embassy_executor::task]
async fn settings_saver_task(flash: &'static SharedFlash) -> ! {
    const MIN_INTERVAL: Duration = Duration::from_secs(60);
    let mut last_save: Option<Instant> = None;
    loop {
        settings::SAVE_SOON.wait().await;
        while let Either::First(()) = select(settings::SAVE_SOON.wait(), Timer::after(Duration::from_secs(1))).await {}
        if let Some(t) = last_save {
            Timer::at(t + MIN_INTERVAL).await;
        }
        let fc = &mut *flash.lock().await;
        let paused = core1::pause().await;
        if let Err(e) = settings::save(&mut fc.flash, &paused, &mut fc.buf, &settings::current()) {
            warn!("settings: saving after ArtAddress failed: {:?}", e);
        }
        last_save = Some(Instant::now());
    }
}


