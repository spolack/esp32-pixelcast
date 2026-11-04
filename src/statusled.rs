//! The status LED: one WS2812 on GPIO16, behind the level shifter.
//!
//! One update is 24 bits (30 us), sent bit-banged with the CPU cycle counter
//! while interrupts are off, so no peripheral is needed for it. States and
//! colours are in `pixelcast_core::statusled`.

pub use pixelcast_core::statusled::*;

/// Sends one colour to a WS2812 (GRB order) on `gpio` (0..32), bit-banged
/// with interrupts off on this core (~30 us).
pub fn send(gpio: u32, rgb: [u8; 3]) {
    const W1TS: *mut u32 = 0x3FF4_4008 as *mut u32;
    const W1TC: *mut u32 = 0x3FF4_400C as *mut u32;
    // At 240 MHz: 0 = 0.35 us high, 1 = 0.7 us high, 1.25 us per bit.
    const T0H: u32 = 84;
    const T1H: u32 = 168;
    const BIT: u32 = 300;
    let mask = 1u32 << gpio;
    let bytes = [rgb[1], rgb[0], rgb[2]];
    critical_section::with(|_| {
        for b in bytes {
            for bit in (0..8).rev() {
                let start = xtensa_lx::timer::get_cycle_count();
                let high = if (b >> bit) & 1 == 1 { T1H } else { T0H };
                // SAFETY: write-1-to-set/clear registers of the GPIO block;
                // only this pin's bit is touched.
                unsafe { W1TS.write_volatile(mask) };
                while xtensa_lx::timer::get_cycle_count().wrapping_sub(start) < high {}
                unsafe { W1TC.write_volatile(mask) };
                while xtensa_lx::timer::get_cycle_count().wrapping_sub(start) < BIT {}
            }
        }
    });
}
