//! How much of the main stack (core 0: all tasks there, interrupts) was ever
//! used. [`paint`] fills the unused part with a pattern at start-up;
//! [`free_min`] finds how far down it has been overwritten since.

const PAINT: u32 = 0xA5A5_A5A5;
/// Left alone at the far end: esp-hal's stack guard word sits there.
const SKIP: u32 = 256;
/// Left alone below the caller's frame (the paint loop's own spills).
const MARGIN: u32 = 1024;

unsafe extern "C" {
    static _stack_end_cpu0: u32;
}

fn low() -> u32 {
    // A linker symbol; only its address is used.
    (&raw const _stack_end_cpu0) as u32 + SKIP
}

/// Call once, early in `main`. Everything below the current frame is unused
/// then (interrupts that run meanwhile only leave dead frames there).
#[inline(never)]
pub fn paint() {
    let here = 0u32;
    let sp = core::ptr::addr_of!(here) as u32;
    let mut a = low();
    while a + 4 <= sp - MARGIN {
        // SAFETY: word-aligned addresses inside the main stack, below every
        // live frame (see above).
        unsafe { core::ptr::write_volatile(a as *mut u32, PAINT) };
        a += 4;
    }
}

/// Bytes of the main stack never used since start (from the guard end).
pub fn free_min() -> u32 {
    let mut a = low();
    // SAFETY: reads inside the main stack.
    while unsafe { core::ptr::read_volatile(a as *const u32) } == PAINT {
        a += 4;
    }
    a - low() + SKIP
}
