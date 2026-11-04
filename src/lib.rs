#![no_std]

extern crate alloc;

pub use pixelcast_core::{artnet, config, frame, http, json, led, mdns, merge, oled, sacn};

pub mod board;
pub mod core1;
pub mod crashlog;
pub mod flashinfo;
pub mod ledout;
pub mod ota;
pub mod settings;
pub mod stackwatch;
pub mod status;
pub mod statusled;
pub mod testpattern;
pub mod web;

/// A zeroed `&'static mut $t` in the reclaimed RAM (internal, DMA-capable;
/// the bootloader's RAM once it is done). Each use is its own buffer and
/// may run only once (checked); `$t` must be valid as all zero bytes.
#[macro_export]
macro_rules! reclaimed_zeroed {
    ($t:ty) => {{
        #[esp_hal::ram(reclaimed)]
        static mut BUF: core::mem::MaybeUninit<$t> = core::mem::MaybeUninit::uninit();
        static TAKEN: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
        assert!(!TAKEN.swap(true, core::sync::atomic::Ordering::AcqRel), "reclaimed buffer taken twice");
        // SAFETY: taken once (checked above), so the reference is unique;
        // the bytes are zeroed first, which the caller vouches is valid.
        unsafe {
            let p = core::ptr::addr_of_mut!(BUF);
            p.cast::<u8>().write_bytes(0, core::mem::size_of::<$t>());
            (*p).assume_init_mut()
        }
    }};
}
