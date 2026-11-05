//! WLED's part of the handoff: the LED setup, in WLED's own codes.
//!
//! When this firmware starts WLED, it writes this record next to the address
//! record (see [`super`], at [`super::SETUP_OFFSET`]). The patched WLED
//! (dualboot/wled/patches) makes it part of its first configuration (first
//! start, or after a factory reset), so WLED drives the same strips: per output the
//! pin, length, LED type, colour order, direction and dark LEDs at the
//! start, and the power limit. Live input (Art-Net, sACN) stays at WLED's
//! defaults: WLED serves another purpose, its user turns that on if wanted.
//!
//! Record (little endian): 0 magic "PXCL", 4 version 1, 5 number of buses,
//! 6..8 zero, 8 mA per LED, 9 zero, 10 current limit in mA (0 = none),
//! 12..20 zero, then 4 buses of 10 bytes (start, length, pin, WLED LED type,
//! WLED colour order, reversed, skipped LEDs, zero), 60 CRC-32 of 0..60.

use crate::led::{LANES, LedType};
use crate::settings::Settings;

pub const LEN: usize = 64;
const _: () = assert!(LEN <= super::SETUP_MAX);
const MAGIC: [u8; 4] = *b"PXCL";

/// WLED's codes (wled00/const.h).
const TYPE_WS2812_RGB: u8 = 22;
const TYPE_SK6812_RGBW: u8 = 30;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Bus {
    /// Position of the bus's first LED in WLED's LED sequence.
    pub start: u16,
    pub len: u16,
    pub pin: u8,
    pub ty: u8,
    pub order: u8,
    pub reversed: bool,
    /// Dark LEDs before the first one that gets data.
    pub skip: u8,
}

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Leds {
    pub buses: heapless::Vec<Bus, LANES>,
    pub ma_per_led: u8,
    pub max_ma: u16,
}

fn order_code(name: [u8; 3]) -> u8 {
    match &name {
        b"grb" => 0,
        b"rgb" => 1,
        b"brg" => 2,
        b"rbg" => 3,
        b"bgr" => 4,
        b"gbr" => 5,
        _ => 0,
    }
}

/// WLED's setup for `s`; `pins[k]` is output k's GPIO. The outputs with
/// LEDs follow each other in WLED's LED sequence.
pub fn from_settings(s: &Settings, pins: [u8; LANES]) -> Leds {
    let mut leds = Leds::default();
    let mut start = 0usize;
    for k in (0..LANES).filter(|&k| s.outputs[k].leds() > 0) {
        let o = &s.outputs[k];
        let skip = usize::from(o.offset).min(o.leds()).min(255);
        let len = o.leds() - skip;
        let _ = leds.buses.push(Bus {
            start: start as u16,
            len: len as u16,
            pin: pins[k],
            ty: if o.ty_or(s.led_type) == LedType::Rgbw { TYPE_SK6812_RGBW } else { TYPE_WS2812_RGB },
            order: order_code(o.order.unwrap_or(s.order).name()),
            reversed: o.reverse,
            skip: skip as u8,
        });
        start += len;
    }
    // WLED's power limiter counts milliamps: at the strip voltage.
    let volts_dv = u32::from(s.led_decivolts.max(1));
    leds.ma_per_led = (s.power_mw_per_led * 10 / volts_dv).min(255) as u8;
    leds.max_ma = (s.power_total_mw * 10 / volts_dv).min(65_000) as u16;
    leds
}

pub fn encode(l: &Leds) -> [u8; LEN] {
    let mut r = [0u8; LEN];
    r[..4].copy_from_slice(&MAGIC);
    r[4] = 1;
    r[5] = l.buses.len() as u8;
    r[8] = l.ma_per_led;
    r[10..12].copy_from_slice(&l.max_ma.to_le_bytes());
    for (i, b) in l.buses.iter().enumerate() {
        let o = 20 + i * 10;
        r[o..o + 2].copy_from_slice(&b.start.to_le_bytes());
        r[o + 2..o + 4].copy_from_slice(&b.len.to_le_bytes());
        r[o + 4] = b.pin;
        r[o + 5] = b.ty;
        r[o + 6] = b.order;
        r[o + 7] = u8::from(b.reversed);
        r[o + 8] = b.skip;
    }
    let crc = crate::inplace::crc32(&r[..60]);
    r[60..64].copy_from_slice(&crc.to_le_bytes());
    r
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use crate::led::Order;

    const PINS: [u8; 4] = [12, 14, 27, 26];

    #[test]
    fn default_setup() {
        // 4 × 512 RGBW, one after the other.
        let l = from_settings(&Settings::default(), PINS);
        let starts: std::vec::Vec<u16> = l.buses.iter().map(|b| b.start).collect();
        assert_eq!(starts, [0, 512, 1024, 1536]);
        assert!(l.buses.iter().all(|b| b.len == 512 && b.ty == TYPE_SK6812_RGBW));
        assert_eq!(l.buses.iter().map(|b| b.pin).collect::<std::vec::Vec<_>>(), PINS);
    }

    #[test]
    fn unused_outputs_types_and_dark_leds() {
        let mut s = Settings::default();
        s.led_type = LedType::Rgb;
        s.order = Order([1, 0, 2]); // grb
        s.outputs[0].leds = 300;
        s.outputs[1].leds = 0;
        s.outputs[2].reverse = true;
        s.outputs[2].offset = 2;
        s.outputs[2].ty = Some(LedType::Rgbw);
        s.outputs[3].leds = 0;
        let l = from_settings(&s, PINS);
        assert_eq!(l.buses.len(), 2);
        assert_eq!(l.buses[0], Bus { start: 0, len: 300, pin: 12, ty: TYPE_WS2812_RGB, order: 0, reversed: false, skip: 0 });
        assert_eq!(l.buses[1], Bus { start: 300, len: 510, pin: 27, ty: TYPE_SK6812_RGBW, order: 0, reversed: true, skip: 2 });
    }

    #[test]
    fn power_in_milliamps_at_the_strip_voltage() {
        let mut s = Settings::default();
        s.power_mw_per_led = 720;
        s.power_total_mw = 480_000;
        s.led_decivolts = 120;
        let l = from_settings(&s, PINS);
        assert_eq!((l.ma_per_led, l.max_ma), (60, 40_000));
    }

    #[test]
    fn record_layout() {
        let r = encode(&from_settings(&Settings::default(), PINS));
        assert_eq!(&r[..6], b"PXCL\x01\x04");
        assert_eq!(u16::from_le_bytes([r[30], r[31]]), 512, "second bus start");
        assert_eq!(u32::from_le_bytes([r[60], r[61], r[62], r[63]]), crate::inplace::crc32(&r[..60]));
    }
}
