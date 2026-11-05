//! The BOOT button: test patterns, setup Wi-Fi, access reset.

use super::*;

/// BOOT button feedback for the status LED: 0 none, 1 past 3 s, 2 past 10 s,
/// 3 starting the other slot.
pub(crate) static BUTTON_STAGE: core::sync::atomic::AtomicU8 = core::sync::atomic::AtomicU8::new(0);

/// BOOT button (the only button the firmware can read; EN restarts the board):
/// - click (under 1 s): next test pattern: rainbow, chase, blackout, off;
/// - three clicks within 2 s: restart into the other firmware slot (same
///   checks as the settings page's Start button): another firmware (e.g.
///   WLED), or another version of this firmware on trial (confirm it there,
///   or the board comes back);
/// - hold 3 s: setup Wi-Fi (status LED turns blue);
/// - hold 10 s: default password and network settings (DHCP), setup Wi-Fi
///   on; the name and LED settings stay (status LED blinks red).
/// A full reset is on the settings page.
#[embassy_executor::task]
pub(crate) async fn button_task(button: Input<'static>, flash: &'static SharedFlash, ctx: &'static web::WebCtx) -> ! {
    let mut clicks: [Option<Instant>; 2] = [None; 2];
    loop {
        while button.is_high() {
            Timer::after(Duration::from_millis(TICK_MS)).await;
        }
        let mut held = 0;
        while button.is_low() {
            Timer::after(Duration::from_millis(TICK_MS)).await;
            held += TICK_MS;
            if held == config::AP_BUTTON_HOLD_MS {
                info!("BOOT held: setup Wi-Fi requested");
                AP_REQUESTED.store(true, Ordering::Relaxed);
                BUTTON_STAGE.store(1, Ordering::Relaxed);
            }
            if held == ACCESS_RESET_HOLD_MS {
                warn!("BOOT held 10 s: resetting password and network settings");
                BUTTON_STAGE.store(2, Ordering::Relaxed);
                let mut s = settings::current();
                s.reset_access();
                let saved = {
                    let fc = &mut *flash.lock().await;
                    let paused = core1::pause().await;
                    settings::save(&mut fc.flash, &paused, &mut fc.buf, &s)
                };
                if let Err(e) = saved {
                    error!("settings: saving the reset failed: {:?}", e);
                }
                settings::set_current(s);
            }
        }
        if held < CLICK_MS && triple_click(&mut clicks) {
            info!("BOOT clicked three times: starting the other slot");
            BUTTON_STAGE.store(3, Ordering::Relaxed);
            testpattern::set(None, 0);
            match web::select_other(ctx).await {
                Ok(_) => {
                    esp32_pixelcast::crashlog::planned("Starting the other slot (BOOT clicked three times)");
                    Timer::after(Duration::from_millis(300)).await;
                    esp_hal::system::software_reset();
                }
                Err(msg) => {
                    warn!("BOOT: other slot not started: {}", msg.trim_end());
                    BUTTON_STAGE.store(0, Ordering::Relaxed);
                }
            }
        } else if held < CLICK_MS {
            let next = match testpattern::current().map(|(p, _)| p) {
                None => Some(testpattern::Pattern::Rainbow),
                Some(testpattern::Pattern::Rainbow) => Some(testpattern::Pattern::Chase),
                Some(testpattern::Pattern::Chase) => Some(testpattern::Pattern::Solid([0; 4])),
                Some(testpattern::Pattern::Solid(_)) => None,
            };
            info!("BOOT click: test pattern {}", next.map_or("off", |p| p.key()));
            testpattern::set(next, Instant::now().as_secs());
        }
        // Let the feedback colour show a moment after release.
        if BUTTON_STAGE.load(Ordering::Relaxed) != 0 {
            Timer::after(Duration::from_millis(1500)).await;
            BUTTON_STAGE.store(0, Ordering::Relaxed);
        }
    }
}

const CLICK_MS: u64 = 1_000;
/// Three clicks within this time start the other slot.
const TRIPLE_MS: u64 = 2_000;

/// Records a click; true if it is the third within `TRIPLE_MS`.
fn triple_click(prev: &mut [Option<Instant>; 2]) -> bool {
    let now = Instant::now();
    let hit = prev[0].is_some_and(|t| now - t <= Duration::from_millis(TRIPLE_MS));
    *prev = if hit { [None; 2] } else { [prev[1], Some(now)] };
    hit
}

const ACCESS_RESET_HOLD_MS: u64 = 10_000;
/// How often the button is read.
const TICK_MS: u64 = 50;
// The held time is compared exactly: the holds must be whole ticks.
const _: () = assert!(config::AP_BUTTON_HOLD_MS % TICK_MS == 0 && ACCESS_RESET_HOLD_MS % TICK_MS == 0);
