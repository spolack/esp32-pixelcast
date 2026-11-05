//! Status LED, OLED and the built-in test pattern player.

use super::*;

/// Drives the status LED (see `statusled`). `_pin` keeps GPIO16 configured
/// as an output; the bits are written through the GPIO registers.
#[embassy_executor::task]
pub(crate) async fn status_led_task(_pin: Output<'static>) -> ! {
    use esp32_pixelcast::statusled::{self, State};
    let mut last = None;
    let mut last_sent = Instant::now();
    loop {
        let eth_link = status::with(|st| st.eth_link);
        let now = Instant::now();
        let state = if BUTTON_STAGE.load(Ordering::Relaxed) == 3 {
            State::StartingOther
        } else if BUTTON_STAGE.load(Ordering::Relaxed) == 2 {
            State::ButtonReset
        } else if BUTTON_STAGE.load(Ordering::Relaxed) == 1 {
            State::ButtonWifi
        } else if web::UPDATING.load(Ordering::Relaxed) {
            State::Updating
        } else if testpattern::active() {
            State::TestPattern
        } else if !artnet_idle_for(Duration::from_secs(2)) {
            State::Receiving
        } else if eth_link {
            State::Ready
        } else if AP_UP.load(Ordering::Relaxed) {
            State::SetupWifi
        } else if now.as_secs() < 8 {
            State::Starting
        } else {
            State::NoNetwork
        };
        let c = statusled::colour(state, now.as_millis());
        // Resend now and then even without a change: a glitch on the line
        // would otherwise stick.
        if Some(c) != last || now - last_sent > Duration::from_secs(1) {
            statusled::send(16, c);
            last = Some(c);
            last_sent = now;
        }
        Timer::after(Duration::from_millis(20)).await;
    }
}

/// Shows name, address, input and output on the OLED, if one answers.
#[embassy_executor::task]
pub(crate) async fn oled_task(mut i2c: esp_hal::i2c::master::I2c<'static, esp_hal::Async>) {
    use esp32_pixelcast::oled::{self, Frame};
    async fn cmds(i2c: &mut esp_hal::i2c::master::I2c<'static, esp_hal::Async>, c: &[u8]) -> bool {
        let mut b = [0u8; 32];
        b[1..=c.len()].copy_from_slice(c); // control byte 0x00: commands
        i2c.write_async(oled::ADDRESS, &b[..=c.len()]).await.is_ok()
    }
    if !cmds(&mut i2c, oled::INIT).await {
        info!("oled: no display at 0x{:02x}; not used", oled::ADDRESS);
        return;
    }
    info!("oled: display found");
    let mut shown: Option<[u8; artnet::DMX_CHANNELS]> = None;
    loop {
        let st = status::get();
        let s = settings::current();
        let l1 = s.name(board::base_mac());
        let l2 = match st.eth_addr {
            Some((ip, p)) => alloc::format!("{}/{}", ip, p),
            None if AP_UP.load(Ordering::Relaxed) => alloc::string::String::from("Wi-Fi 192.168.4.1"),
            None => alloc::string::String::from("No network"),
        };
        let l3 = if let Some((p, _)) = testpattern::current() {
            alloc::format!("Test pattern: {}", p.key())
        } else {
            match st.artnet {
                Some(r) if r.universes > 0 => alloc::format!("{} {}fps {}u", s.protocol.key(), r.fps_max, r.universes),
                _ => alloc::format!("{}: no data", s.protocol.key()),
            }
        };
        let l4 = if web::UPDATING.load(Ordering::Relaxed) {
            alloc::string::String::from("Updating firmware...")
        } else {
            alloc::format!("LED {}fps under {}", st.led_fps, st.led_underruns)
        };
        let f = Frame::text(&[&l1, &l2, &l3, &l4]);
        if shown != Some(f.buf) {
            let mut ok = cmds(&mut i2c, oled::WINDOW).await;
            for c in f.chunks() {
                if !ok {
                    break;
                }
                let mut b = [0x40u8; 33]; // control byte 0x40: data
                b[1..=c.len()].copy_from_slice(c);
                ok = i2c.write_async(oled::ADDRESS, &b[..=c.len()]).await.is_ok();
            }
            if ok {
                shown = Some(f.buf);
            }
        }
        Timer::after(Duration::from_millis(500)).await;
    }
}

/// Plays the built-in test pattern while one is selected (30 frames/s),
/// as universe data through the normal LED path.
#[embassy_executor::task]
pub(crate) async fn test_pattern_task() -> ! {
    let mut frame = 0u32;
    let mut buf = [0u8; artnet::DMX_CHANNELS];
    loop {
        // Signal loss with "show colour": the same generator plays a solid
        // colour until data arrives again (the receive task keeps feeding
        // the output, so new data takes over at once).
        // Copies of the few settings used here (not all of them, per frame).
        let (loss, loss_after_s, loss_color) = settings::with(|s| (s.loss, s.loss_after_s, s.loss_color));
        let loss_scene = loss == settings::Loss::Color
            && !testpattern::active()
            && artnet_idle_for(Duration::from_secs(u64::from(loss_after_s)));
        let (p, started) = match testpattern::current() {
            Some(c) => c,
            None if loss_scene => (testpattern::Pattern::Solid(loss_color), Instant::now().as_secs()),
            None => {
                let _ = select(testpattern::CHANGED.wait(), Timer::after(Duration::from_millis(500))).await;
                continue;
            }
        };
        if Instant::now().as_secs().saturating_sub(started) >= testpattern::AUTO_OFF_S {
            info!("test pattern: switched off after {} min", testpattern::AUTO_OFF_S / 60);
            testpattern::set(None, 0);
            continue;
        }
        let (outputs, led_type) = settings::with(|s| (s.outputs, s.led_type));
        for o in outputs.iter() {
            let ch = o.ty_or(led_type).channels();
            let pixels = o.pixels();
            for k in 0..o.universes(led_type) {
                let (first, n, at) = o.span(led_type, k);
                let data = &mut buf[..at + n * ch];
                data[..at].fill(0);
                testpattern::fill(p, frame, first, pixels, ch, &mut data[at..]);
                ledout::on_dmx(o.universe.wrapping_add(k as u16) & 0x7fff, data);
            }
        }
        frame = frame.wrapping_add(1);
        let _ = select(Timer::after(Duration::from_millis(33)), testpattern::CHANGED.wait()).await;
    }
}
