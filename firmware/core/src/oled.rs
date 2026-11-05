//! Optional SSD1306 OLED (128 × 32, I2C address 0x3C) showing four lines of
//! status. The I2C transfers are async and short, so the network tasks on
//! the same core never wait for the display.

use embedded_graphics::{
    mono_font::{MonoTextStyle, ascii::FONT_5X8},
    pixelcolor::BinaryColor,
    prelude::*,
    text::{Baseline, Text},
};

pub const ADDRESS: u8 = 0x3C;
pub const WIDTH: usize = 128;
pub const HEIGHT: usize = 32;
const PAGES: usize = HEIGHT / 8;
/// Data bytes per I2C transfer (plus the control byte).
const CHUNK: usize = 32;

/// Initialisation for a 128 × 32 panel with the charge pump on, horizontal
/// addressing (the whole buffer in one stream).
pub const INIT: &[u8] = &[
    0xAE, // display off
    0xD5, 0x80, // clock
    0xA8, 0x1F, // multiplex: 32 rows
    0xD3, 0x00, // no display offset
    0x40, // start line 0
    0x8D, 0x14, // charge pump on
    0x20, 0x00, // horizontal addressing
    0xA1, // column 127 is SEG0 (mounted rotated)
    0xC8, // scan from COM[N-1]
    0xDA, 0x02, // COM pins for 128 × 32
    0x81, 0x8F, // contrast
    0xD9, 0xF1, // pre-charge
    0xDB, 0x40, // VCOMH
    0xA4, // show RAM
    0xA6, // not inverted
    0xAF, // display on
];

/// The picture, one byte per 8 vertical pixels (page layout of the SSD1306).
pub struct Frame {
    pub buf: [u8; WIDTH * PAGES],
}

impl Default for Frame {
    fn default() -> Self {
        Self { buf: [0; WIDTH * PAGES] }
    }
}

impl OriginDimensions for Frame {
    fn size(&self) -> Size {
        Size::new(WIDTH as u32, HEIGHT as u32)
    }
}

impl DrawTarget for Frame {
    type Color = BinaryColor;
    type Error = core::convert::Infallible;

    fn draw_iter<I: IntoIterator<Item = Pixel<BinaryColor>>>(&mut self, pixels: I) -> Result<(), Self::Error> {
        for Pixel(p, c) in pixels {
            let (Ok(x), Ok(y)) = (usize::try_from(p.x), usize::try_from(p.y)) else {
                continue;
            };
            if x >= WIDTH || y >= HEIGHT {
                continue;
            }
            let i = (y / 8) * WIDTH + x;
            if c.is_on() {
                self.buf[i] |= 1 << (y % 8);
            } else {
                self.buf[i] &= !(1 << (y % 8));
            }
        }
        Ok(())
    }
}

impl Frame {
    /// Draws up to four lines of text (5 × 8 font, 25 characters a line).
    pub fn text(lines: &[&str]) -> Self {
        let mut f = Frame::default();
        let style = MonoTextStyle::new(&FONT_5X8, BinaryColor::On);
        for (i, line) in lines.iter().take(PAGES).enumerate() {
            let _ = Text::with_baseline(line, Point::new(0, (i * 8) as i32), style, Baseline::Top).draw(&mut f);
        }
        f
    }

    /// The I2C writes that put this frame on the panel: the address window,
    /// then the data in short chunks (control byte 0x40 first).
    pub fn chunks(&self) -> impl Iterator<Item = &[u8]> {
        self.buf.chunks(CHUNK)
    }
}

/// Commands that select the whole panel as the write window.
pub const WINDOW: &[u8] = &[0x21, 0, (WIDTH - 1) as u8, 0x22, 0, (PAGES - 1) as u8];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_sets_pixels_in_the_right_page() {
        let f = Frame::text(&["", "", "", "I"]);
        // Only the last page (rows 24..32) has anything lit.
        assert!(f.buf[..3 * WIDTH].iter().all(|&b| b == 0));
        assert!(f.buf[3 * WIDTH..].iter().any(|&b| b != 0));
        assert_eq!(f.chunks().count(), WIDTH * PAGES / CHUNK);
    }
}
