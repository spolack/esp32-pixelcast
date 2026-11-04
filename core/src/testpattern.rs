//! Built-in test patterns: generated as ordinary universe data, so colour
//! order, brightness and the output mapping apply exactly as for Art-Net.

/// A test pattern; `None` in [`current`] means normal input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pattern {
    /// A rainbow moving along each output.
    Rainbow,
    /// One full-white LED moving one LED per frame.
    Chase,
    /// Every LED in one colour (R, G, B, W).
    Solid([u8; 4]),
}

impl Pattern {
    pub fn key(self) -> &'static str {
        match self {
            Pattern::Rainbow => "rainbow",
            Pattern::Chase => "chase",
            Pattern::Solid(_) => "solid",
        }
    }
}

/// A pattern switches itself off after this long.
pub const AUTO_OFF_S: u64 = 30 * 60;

/// HSV hue (0..360) at full saturation and value, as RGB.
fn hue(h: u32) -> [u8; 3] {
    let h = h % 360;
    let x = (255 * (60 - (h % 120).abs_diff(60)) / 60) as u8;
    match h / 60 {
        0 => [255, x, 0],
        1 => [x, 255, 0],
        2 => [0, 255, x],
        3 => [0, x, 255],
        4 => [x, 0, 255],
        _ => [255, 0, x],
    }
}

/// Fills one universe of DMX data (channel order R, G, B[, W]).
/// `first_led` = index of the universe's first LED within its output,
/// `leds` = LEDs of the output, `channels` = 3 or 4, `frame` counts up.
pub fn fill(p: Pattern, frame: u32, first_led: usize, leds: usize, channels: usize, out: &mut [u8]) {
    out.fill(0);
    let per = out.len() / channels;
    for i in 0..per {
        let led = first_led + i;
        if led >= leds {
            break;
        }
        let px: [u8; 4] = match p {
            Pattern::Rainbow => {
                let [r, g, b] = hue((led as u32 * 360 / leds.max(1) as u32) + frame * 4);
                [r, g, b, 0]
            }
            Pattern::Chase => {
                if led == frame as usize % leds.max(1) {
                    [255, 255, 255, 255]
                } else {
                    [0; 4]
                }
            }
            Pattern::Solid(c) => c,
        };
        out[i * channels..i * channels + channels].copy_from_slice(&px[..channels]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chase_lights_one_led_across_universes() {
        // 300 RGBW LEDs: universe 0 holds LEDs 0..128, universe 1 128..256.
        let mut u0 = [0u8; 512];
        let mut u1 = [0u8; 512];
        fill(Pattern::Chase, 130, 0, 300, 4, &mut u0);
        fill(Pattern::Chase, 130, 128, 300, 4, &mut u1);
        assert!(u0.iter().all(|&b| b == 0));
        assert_eq!(&u1[8..12], &[255; 4]);
        assert_eq!(u1.iter().filter(|&&b| b != 0).count(), 4);
    }

    #[test]
    fn solid_rgb_and_stops_after_last_led() {
        let mut u = [0u8; 510];
        fill(Pattern::Solid([1, 2, 3, 4]), 0, 0, 2, 3, &mut u);
        assert_eq!(&u[..9], &[1, 2, 3, 1, 2, 3, 0, 0, 0]);
    }

    #[test]
    fn hue_wheel() {
        assert_eq!(hue(0), [255, 0, 0]);
        assert_eq!(hue(120), [0, 255, 0]);
        assert_eq!(hue(240), [0, 0, 255]);
        assert_eq!(hue(60), [255, 255, 0]);
    }
}
