//! The parts of the firmware that need no hardware: they build and are
//! tested on the host (`tools/test.sh`). The firmware re-exports them
//! (`esp32_pixelcast::led` etc.).
#![no_std]

extern crate alloc;

pub mod artnet;
pub mod audio;
pub mod config;
pub mod dhcp;
pub mod dualboot;
pub mod frame;
pub mod http;
pub mod inplace;
pub mod json;
pub mod led;
pub mod mdns;
pub mod merge;
pub mod oled;
pub mod sacn;
pub mod settings;
pub mod statusled;
pub mod testpattern;

