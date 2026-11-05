//! LED output logic that does not touch hardware (and has host tests):
//! bit timing and its quantization to I2S samples, the sample encoder, and
//! the Art-Net universe → LED mapping.
//!
//! ## How the signal is made
//! The I2S peripheral in 8-bit parallel mode clocks out one byte per sample;
//! bit `k` of each byte drives output `k` (SIG1 = bit 0 … SIG4 = bit 3). One
//! LED data bit takes `n` samples: the first `h0` are high for every lane,
//! samples `h0..h1` carry the bit (high = 1), the rest are low. So a 0 is a
//! `h0`-sample pulse and a 1 a `h1`-sample pulse, and every bit takes the
//! same time on all four lanes, as one shared clock requires.

/// Number of LED outputs.
use crate::artnet::DMX_CHANNELS;

pub const LANES: usize = 4;
/// Most LEDs one output may be set to. What really fits is set by the
/// shared LED memory ([`effective`]): all outputs together.
pub const MAX_LEDS: usize = 4096;
/// Largest samples-per-bit the encoder supports.
pub const MAX_SAMPLES_PER_BIT: u8 = 8;
/// Highest I2S sample rate used; encoding cost grows with it.
pub const MAX_SAMPLE_RATE_HZ: u32 = 8_000_000;

/// Requested bit timing, in nanoseconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timing {
    /// High time of a 0 bit.
    pub t0h_ns: u32,
    /// High time of a 1 bit.
    pub t1h_ns: u32,
    /// Duration of every bit.
    pub period_ns: u32,
}

impl Timing {
    /// SK6812 datasheet values: 0.3 / 0.6 µs high, 1.2 µs per bit.
    pub const SK6812: Timing = Timing { t0h_ns: 300, t1h_ns: 600, period_ns: 1200 };

    /// Checks the order and the minimum low time; returns an error text.
    pub fn validate(&self) -> Result<(), &'static str> {
        if !(100..=1000).contains(&self.t0h_ns) {
            return Err("The short pulse must be 100 to 1000 ns.");
        }
        if !(150..=2000).contains(&self.t1h_ns) {
            return Err("The long pulse must be 150 to 2000 ns.");
        }
        if !(500..=3000).contains(&self.period_ns) {
            return Err("The bit time must be 500 to 3000 ns.");
        }
        if self.t1h_ns <= self.t0h_ns {
            return Err("The long pulse must be longer than the short pulse.");
        }
        if self.period_ns < self.t1h_ns + 100 {
            return Err("The bit time must leave at least 100 ns low after the long pulse.");
        }
        Ok(())
    }
}

/// The timing as the hardware produces it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Shape {
    /// Samples per bit.
    pub n: u8,
    /// Samples high for a 0.
    pub h0: u8,
    /// Samples high for a 1.
    pub h1: u8,
    /// I2S sample rate to request.
    pub sample_rate_hz: u32,
}

impl Shape {
    /// One sample, in picoseconds (for display).
    pub fn sample_ps(&self) -> u64 {
        1_000_000_000_000 / u64::from(self.sample_rate_hz)
    }

    /// The actual timing in nanoseconds (rounded): (t0h, t1h, period).
    pub fn actual_ns(&self) -> (u32, u32, u32) {
        let s = self.sample_ps();
        let ns = |k: u8| ((u64::from(k) * s + 500) / 1000) as u32;
        (ns(self.h0), ns(self.h1), ns(self.n))
    }
}

/// Samples per bit the encoder handles at full speed (one or two words per
/// bit). Other counts fall back to a byte-wise path that cannot keep up with
/// long strips (measured: 7 samples/bit cut every frame short), so they are
/// never chosen.
pub const SAMPLES_PER_BIT_CHOICES: [u8; 2] = [4, 8];

/// Picks samples-per-bit (4 or 8) and pulse lengths that come closest to
/// `t`: 4 if its worst pulse error is at most 30 ns, else whichever is
/// closer. Pulses therefore land within half a step (a quarter or an eighth
/// of the bit time) of the request. The bit time is kept exact by choosing
/// the sample rate as `n / period`. `web/config.html` mirrors this; keep them
/// in step.
pub fn quantize(t: &Timing) -> Shape {
    const GOOD_ENOUGH_PS: u64 = 30_000;
    let mut best: Option<(u64, Shape)> = None;
    for n in SAMPLES_PER_BIT_CHOICES {
        let rate = (u64::from(n) * 1_000_000_000 / u64::from(t.period_ns)) as u32;
        if rate > MAX_SAMPLE_RATE_HZ {
            break;
        }
        let sample_ps = u64::from(t.period_ns) * 1000 / u64::from(n);
        let round = |ns: u32| ((u64::from(ns) * 1000 + sample_ps / 2) / sample_ps) as u8;
        let h0 = round(t.t0h_ns).clamp(1, n - 2);
        let h1 = round(t.t1h_ns).clamp(h0 + 1, n - 1);
        let err = |k: u8, ns: u32| (u64::from(k) * sample_ps).abs_diff(u64::from(ns) * 1000);
        let worst = err(h0, t.t0h_ns).max(err(h1, t.t1h_ns));
        let shape = Shape { n, h0, h1, sample_rate_hz: rate };
        if worst <= GOOD_ENOUGH_PS {
            return shape;
        }
        if best.is_none_or(|(e, _)| worst < e) {
            best = Some((worst, shape));
        }
    }
    best.map(|(_, s)| s).unwrap_or(Shape { n: 4, h0: 1, h1: 2, sample_rate_hz: 3_333_333 })
}

/// Maximum frame rate for `leds` LEDs of `bytes_per_led` channels, with a
/// latch pause of `latch_us`, at the given bit time.
pub fn max_fps(leds: usize, bytes_per_led: usize, period_ns: u32, latch_us: u32) -> u32 {
    let frame_ns = leds as u64 * bytes_per_led as u64 * 8 * u64::from(period_ns) + u64::from(latch_us) * 1000;
    if frame_ns == 0 {
        return 0;
    }
    (1_000_000_000 / frame_ns) as u32
}

// ── Render: blending, gain, dithering ───────────────────────────────────────

/// Gain 1.0 (and blend weight "current only").
pub const UNITY: u16 = 256;

/// How the LED core turns the input frames into the frame it sends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Render {
    /// Weight of the current frame over the previous one, 0..=UNITY
    /// (interpolation; UNITY = current only).
    pub t: u16,
    /// Gain per output, 0..=UNITY (brightness × power limit × fade).
    pub gain: [u16; LANES],
    /// Temporal dithering instead of plain rounding.
    pub dither: bool,
    /// Frame counter; moves the dither pattern.
    pub frame: u32,
}

impl Render {
    pub const IDENTITY: Render = Render { t: UNITY, gain: [UNITY; LANES], dither: false, frame: 0 };

    pub fn is_identity(&self) -> bool {
        self.t >= UNITY && self.gain.iter().all(|&g| g >= UNITY) && !self.dither
    }

    /// One output byte from the previous and current input bytes of lane
    /// `k` at byte position `pos`. Works in 8.8 fixed point; dithering adds
    /// a pseudo-random threshold that changes every frame, so the average
    /// over a few frames keeps the fraction.
    #[inline(always)]
    pub fn byte(&self, prev: u8, cur: u8, k: usize, pos: usize) -> u8 {
        let t = u32::from(self.t.min(UNITY));
        // 0..=255 * 256: the blended value in 8.8.
        let v = u32::from(prev) * (256 - t) + u32::from(cur) * t;
        let v = v * u32::from(self.gain[k].min(UNITY)) / 256;
        let d = if self.dither {
            let mut h = (pos as u32).wrapping_mul(0x9E37_79B1) ^ (k as u32).wrapping_mul(0x85EB_CA6B) ^ self.frame.wrapping_mul(0xC2B2_AE35);
            h ^= h >> 15;
            h = h.wrapping_mul(0x2C1B_3C6D);
            (h >> 24) & 0xff
        } else {
            128
        };
        ((v + d) >> 8).min(255) as u8
    }
}

/// Power model and limits, all in mA. 0 = no limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PowerLimit {
    /// Power of one channel at full value per output (mW): the LED's power
    /// at full white divided by its channels (3 RGB, 4 RGBW).
    pub mw_per_channel: [u32; LANES],
    /// Most power per output (e.g. its DC-DC converter); 0 = no limit.
    pub per_output_mw: u32,
    /// Most power for all outputs together (the supply); 0 = no limit.
    pub total_mw: u32,
}

impl PowerLimit {
    /// From the power of one LED at full white and each output's channels.
    pub fn from_led(mw_per_led: u32, channels: [usize; LANES], per_output_mw: u32, total_mw: u32) -> Self {
        let mw_per_channel = core::array::from_fn(|k| mw_per_led / channels[k].max(1) as u32);
        PowerLimit { mw_per_channel, per_output_mw, total_mw }
    }
}

/// Gains per output from the brightness (0..=UNITY), the power limit and
/// the channel-value sums of the frame per output. Returns the gains and
/// the estimated power per output after limiting (mW).
pub fn lane_gains(brightness: u16, sums: &[u32; LANES], p: &PowerLimit) -> ([u16; LANES], [u32; LANES]) {
    let mut gain = [brightness.min(UNITY); LANES];
    // Power at the given gain: sum / 255 channels-at-full × mW.
    let mw = |k: usize, g: u16| {
        (u64::from(sums[k]) * u64::from(p.mw_per_channel[k]) * u64::from(g) / (255 * u64::from(UNITY))) as u32
    };
    if p.per_output_mw > 0 {
        for k in 0..LANES {
            let w = mw(k, gain[k]);
            if w > p.per_output_mw {
                gain[k] = (u64::from(gain[k]) * u64::from(p.per_output_mw) / u64::from(w)) as u16;
            }
        }
    }
    let mut power: [u32; LANES] = core::array::from_fn(|k| mw(k, gain[k]));
    let total: u32 = power.iter().sum();
    if p.total_mw > 0 && total > p.total_mw {
        for k in 0..LANES {
            gain[k] = (u64::from(gain[k]) * u64::from(p.total_mw) / u64::from(total)) as u16;
        }
        power = core::array::from_fn(|k| mw(k, gain[k]));
    }
    (gain, power)
}

// ── Encoder ────────────────────────────────────────────────────────────────

/// Streams the I2S samples of one frame in pieces of any size (multiples
/// of 4 bytes). `lanes[k]` holds output `k`'s bytes in wire order (colour
/// order and brightness already applied).
pub struct Encoder<'a> {
    lanes: [&'a [u8]; LANES],
    /// The previous input frame and how to mix it in (see [`Render`]);
    /// `None` sends `lanes` as they are.
    render: Option<([&'a [u8]; LANES], Render)>,
    /// Byte positions to send (the longest lane).
    len: usize,
    /// Lanes that carry data (bit k = output k).
    mask: u8,
    shape: Shape,
    pos: usize,
    /// Latch samples still to send (a multiple of 4).
    latch_left: usize,
    /// One encoded byte position, already swapped, partly handed out.
    pending: [u8; 8 * MAX_SAMPLES_PER_BIT as usize],
    pend_off: usize,
    pend_len: usize,
    /// Fast path (samples per bit a multiple of 4): each group of four
    /// samples of a bit is one word `base[g] | v * vmul[g]`, already in the
    /// swapped order, where `v` is the bit of every lane.
    fast: bool,
    base: [u32; MAX_SAMPLES_PER_BIT as usize / 4],
    vmul: [u32; MAX_SAMPLES_PER_BIT as usize / 4],
}

/// `SPREAD[b]` has bit `7 - j` of `b` in bit `4 * j` (j = 0..8), so OR-ing
/// `SPREAD[lane k byte] << k` over the lanes gives, in nibble j, the sample
/// value of LED bit j (most significant bit first) for all lanes at once.
static SPREAD: [u32; 256] = {
    let mut t = [0u32; 256];
    let mut b = 0;
    while b < 256 {
        let mut j = 0;
        while j < 8 {
            if (b >> (7 - j)) & 1 == 1 {
                t[b] |= 1 << (4 * j);
            }
            j += 1;
        }
        b += 1;
    }
    t
};

impl<'a> Encoder<'a> {
    pub fn new(lanes: [&'a [u8]; LANES], shape: Shape, latch_us: u32) -> Self {
        let len = lanes.iter().map(|l| l.len()).max().unwrap_or(0);
        let mut mask = 0u8;
        for (k, l) in lanes.iter().enumerate() {
            if !l.is_empty() {
                mask |= 1 << k;
            }
        }
        let latch = (u64::from(latch_us) * u64::from(shape.sample_rate_hz)).div_ceil(1_000_000) as usize;
        let n = usize::from(shape.n);
        let fast = n % 4 == 0;
        let mut base = [0u32; MAX_SAMPLES_PER_BIT as usize / 4];
        let mut vmul = [0u32; MAX_SAMPLES_PER_BIT as usize / 4];
        if fast {
            for g in 0..n / 4 {
                // Memory byte m of the group holds sample [2, 3, 0, 1][m]
                // (see `write_swapped`); a little-endian word puts byte m in
                // bits 8m..8m+8.
                for (m, &off) in [2usize, 3, 0, 1].iter().enumerate() {
                    let sample = 4 * g + off;
                    if sample < usize::from(shape.h0) {
                        base[g] |= u32::from(mask) << (8 * m);
                    } else if sample < usize::from(shape.h1) {
                        vmul[g] |= 1 << (8 * m);
                    }
                }
            }
        }
        Self {
            lanes,
            render: None,
            len,
            mask,
            shape,
            pos: 0,
            latch_left: latch.div_ceil(4) * 4,
            pending: [0; 8 * MAX_SAMPLES_PER_BIT as usize],
            pend_off: 0,
            pend_len: 0,
            fast,
            base,
            vmul,
        }
    }

    /// Mixes each byte while encoding: `prev` blended into the lanes by
    /// `r.t`, scaled by the lane's gain, dithered (see [`Render::byte`]).
    /// An identity render (full weight, full gain, no dither) costs nothing.
    pub fn with_render(mut self, prev: [&'a [u8]; LANES], r: Render) -> Self {
        if !r.is_identity() {
            self.render = Some((prev, r));
        }
        self
    }

    /// The byte of lane `k` at `pos`, rendered.
    #[inline(always)]
    fn byte(&self, k: usize, pos: usize) -> u8 {
        let cur = self.lanes[k].get(pos).copied().unwrap_or(0);
        match &self.render {
            None => cur,
            Some((prev, r)) => r.byte(prev[k].get(pos).copied().unwrap_or(0), cur, k, pos),
        }
    }

    /// Turns the fast path off (tests compare both paths).
    #[cfg(test)]
    fn slow(mut self) -> Self {
        self.fast = false;
        self
    }

    /// Encodes byte position `pos` straight into `out` (`8 * n` bytes).
    #[inline(always)]
    fn encode_fast(&self, pos: usize, out: &mut [u8]) {
        let mut x = 0u32;
        for k in 0..LANES {
            x |= SPREAD[usize::from(self.byte(k, pos))] << k;
        }
        let groups = usize::from(self.shape.n) / 4;
        let mask = u32::from(self.mask);
        let mut words = out.chunks_exact_mut(4);
        for j in 0..8 {
            let v = (x >> (4 * j)) & mask;
            for g in 0..groups {
                if let Some(w) = words.next() {
                    w.copy_from_slice(&(self.base[g] | v * self.vmul[g]).to_le_bytes());
                }
            }
        }
    }

    /// Bytes one source byte position turns into (always a multiple of 4).
    pub fn bytes_per_position(&self) -> usize {
        8 * usize::from(self.shape.n)
    }

    /// Total bytes of the frame, latch included (before any `fill`).
    pub fn total_bytes(&self) -> usize {
        self.len * self.bytes_per_position() + self.latch_left
    }

    pub fn done(&self) -> bool {
        self.pos >= self.len && self.pend_len == 0 && self.latch_left == 0
    }

    /// Encodes the next byte position into `pending`.
    fn encode_next(&mut self) {
        let n = usize::from(self.shape.n);
        let (h0, h1) = (usize::from(self.shape.h0), usize::from(self.shape.h1));
        let mut b = [0u8; LANES];
        for (k, v) in b.iter_mut().enumerate() {
            *v = self.byte(k, self.pos);
        }
        let mut tmp = [0u8; 8 * MAX_SAMPLES_PER_BIT as usize];
        let mut i = 0;
        // Bit 7 first; lane k's bit goes to bit k of the sample.
        for bit in (0..8).rev() {
            let mut v = 0u8;
            for (k, &byte) in b.iter().enumerate() {
                v |= ((byte >> bit) & 1) << k;
            }
            let v = v & self.mask;
            for s in 0..n {
                tmp[i] = if s < h0 {
                    self.mask
                } else if s < h1 {
                    v
                } else {
                    0
                };
                i += 1;
            }
        }
        write_swapped(&tmp[..8 * n], &mut self.pending[..8 * n]);
        self.pend_off = 0;
        self.pend_len = 8 * n;
        self.pos += 1;
    }

    /// Fills `dst` (rounded down to a multiple of 4); returns bytes written.
    pub fn fill(&mut self, dst: &mut [u8]) -> usize {
        let cap = dst.len() / 4 * 4;
        let mut written = 0;
        while written < cap {
            if self.pend_len > 0 {
                let k = self.pend_len.min(cap - written);
                dst[written..written + k].copy_from_slice(&self.pending[self.pend_off..self.pend_off + k]);
                self.pend_off += k;
                self.pend_len -= k;
                written += k;
            } else if self.pos < self.len {
                let bpp = self.bytes_per_position();
                if self.fast && cap - written >= bpp {
                    // Whole positions straight into the DMA buffer.
                    while self.pos < self.len && cap - written >= bpp {
                        self.encode_fast(self.pos, &mut dst[written..written + bpp]);
                        self.pos += 1;
                        written += bpp;
                    }
                } else {
                    self.encode_next();
                }
            } else if self.latch_left > 0 {
                let k = self.latch_left.min(cap - written);
                dst[written..written + k].fill(0);
                self.latch_left -= k;
                written += k;
            } else {
                break;
            }
        }
        written
    }
}

/// Copies samples into the DMA buffer in the order the 8-bit mode needs:
/// it outputs `[A, B, C, D]` as `[C, D, A, B]`, so each group of four is
/// stored pre-swapped. `src.len()` is a multiple of 4.
pub fn write_swapped(src: &[u8], dst: &mut [u8]) {
    for (s, d) in src.chunks_exact(4).zip(dst.chunks_exact_mut(4)) {
        d[0] = s[2];
        d[1] = s[3];
        d[2] = s[0];
        d[3] = s[1];
    }
}

/// The inverse of [`write_swapped`] + encoding, for tests: decodes the
/// samples of lane `k` back into bytes using the shape.
pub fn decode_lane(dma: &[u8], shape: Shape, lane: usize, positions: usize) -> alloc::vec::Vec<u8> {
    let mut samples = alloc::vec![0u8; dma.len()];
    write_swapped(dma, &mut samples); // the swap is its own inverse
    let n = usize::from(shape.n);
    let mut out = alloc::vec::Vec::new();
    for p in 0..positions {
        let mut byte = 0u8;
        for bit in 0..8 {
            let start = (p * 8 + bit) * n;
            let high = samples[start..start + n].iter().take_while(|s| (*s >> lane) & 1 == 1).count();
            byte = (byte << 1) | u8::from(high == usize::from(shape.h1));
        }
        out.push(byte);
    }
    out
}

// ── Mapping ────────────────────────────────────────────────────────────────

/// LED type: channels per LED in the DMX data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LedType {
    Rgb,
    Rgbw,
}

impl LedType {
    /// The settings/API key: "rgb" or "rgbw".
    pub fn key(self) -> &'static str {
        match self {
            LedType::Rgb => "rgb",
            LedType::Rgbw => "rgbw",
        }
    }

    pub fn channels(self) -> usize {
        match self {
            LedType::Rgb => 3,
            LedType::Rgbw => 4,
        }
    }

    /// LEDs per universe (whole LEDs only).
    pub fn per_universe(self) -> usize {
        DMX_CHANNELS / self.channels()
    }
}

/// Colour order on the wire, as indices into the DMX order R, G, B (W is
/// always sent last). "grb" = green first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Order(pub [u8; 3]);

impl Order {
    pub fn parse(s: &str) -> Option<Order> {
        let b = s.as_bytes();
        if b.len() != 3 {
            return None;
        }
        let mut idx = [0u8; 3];
        for (i, c) in b.iter().enumerate() {
            idx[i] = match c.to_ascii_lowercase() {
                b'r' => 0,
                b'g' => 1,
                b'b' => 2,
                _ => return None,
            };
        }
        let mut seen = [false; 3];
        for &i in &idx {
            if core::mem::replace(&mut seen[usize::from(i)], true) {
                return None;
            }
        }
        Some(Order(idx))
    }

    pub fn name(self) -> [u8; 3] {
        self.0.map(|i| b"rgb"[usize::from(i)])
    }
}

/// One output's configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutputCfg {
    pub leds: u16,
    /// First universe (port-address).
    pub universe: u16,
    /// LED type; `None` = the global type.
    pub ty: Option<LedType>,
    /// Colour order; `None` = the global order.
    pub order: Option<Order>,
    /// The strip is fed from its far end: the first pixel goes to the last LED.
    pub reverse: bool,
    /// LEDs per DMX pixel (1 = each LED its own pixel).
    pub group: u8,
    /// LEDs at the start of the strip that stay dark.
    pub offset: u16,
    /// DMX channel (1..=512) of the first pixel in the first universe; the
    /// following universes start at channel 1.
    pub start: u16,
}

impl OutputCfg {
    pub const fn new(leds: u16, universe: u16) -> Self {
        Self { leds, universe, ty: None, order: None, reverse: false, group: 1, offset: 0, start: 1 }
    }

    pub fn ty_or(&self, global: LedType) -> LedType {
        self.ty.unwrap_or(global)
    }

    /// LEDs on the strip (capped).
    pub fn leds(&self) -> usize {
        usize::from(self.leds).min(MAX_LEDS)
    }

    /// DMX pixels this output takes.
    pub fn pixels(&self) -> usize {
        self.leds().saturating_sub(usize::from(self.offset)).div_ceil(usize::from(self.group.max(1)))
    }

    /// Byte offset of the first pixel in the first universe (from `start`,
    /// kept low enough for at least one whole pixel).
    pub fn start_byte(&self, global: LedType) -> usize {
        usize::from(self.start.max(1) - 1).min(DMX_CHANNELS - self.ty_or(global).channels())
    }

    /// Universe `k` of this output: (first pixel, pixels, byte offset of the
    /// first pixel in the DMX data).
    pub fn span(&self, global: LedType, k: usize) -> (usize, usize, usize) {
        let ty = self.ty_or(global);
        let skip = self.start_byte(global);
        let first_cap = (DMX_CHANNELS - skip) / ty.channels();
        let pixels = self.pixels();
        let (first, at) = if k == 0 { (0, skip) } else { (first_cap + (k - 1) * ty.per_universe(), 0) };
        let cap = if k == 0 { first_cap } else { ty.per_universe() };
        (first.min(pixels), pixels.saturating_sub(first).min(cap), at)
    }

    /// Universes this output takes (from `universe` on).
    pub fn universes(&self, global: LedType) -> usize {
        let pixels = self.pixels();
        let first_cap = (DMX_CHANNELS - self.start_byte(global)) / self.ty_or(global).channels();
        if pixels == 0 {
            0
        } else if pixels <= first_cap {
            1
        } else {
            1 + (pixels - first_cap).div_ceil(self.ty_or(global).per_universe())
        }
    }

    /// Bytes this output sends per frame.
    pub fn wire_bytes(&self, global: LedType) -> usize {
        self.leds() * self.ty_or(global).channels()
    }
}

/// Where a universe's data goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Slot {
    pub lane: usize,
    /// First DMX pixel (of the output) in this universe.
    pub first_pixel: usize,
    /// Pixels in this universe.
    pub pixels: usize,
    /// Bit in the "received" mask.
    pub bit: u32,

    pub ty: LedType,
    pub order: Order,
    pub reverse: bool,
    pub group: usize,
    pub offset: usize,
    /// LEDs on the output.
    pub leds: usize,
    /// Byte offset of this universe's first pixel in the DMX data.
    pub dmx_start: usize,
}

/// The universes the outputs need, at most 48 in total.
#[derive(Debug, Clone)]
pub struct Mapping {
    pub slots: heapless_slots::Slots,
}

/// A tiny fixed-capacity list without another dependency.
pub mod heapless_slots {
    use super::Slot;
    pub const CAP: usize = 48;
    #[derive(Debug, Clone)]
    pub struct Slots {
        pub items: [(u16, Slot); CAP],
        pub len: usize,
    }
    impl Slots {
        pub fn iter(&self) -> impl Iterator<Item = &(u16, Slot)> {
            self.items[..self.len].iter()
        }
    }
}

impl Mapping {
    pub fn new(global_ty: LedType, global_order: Order, outputs: &[OutputCfg; LANES]) -> Mapping {
        let empty = Slot {
            lane: 0,
            first_pixel: 0,
            pixels: 0,
            bit: 0,
            ty: global_ty,
            order: global_order,
            reverse: false,
            group: 1,
            offset: 0,
            leds: 0,
            dmx_start: 0,
        };
        let mut slots = heapless_slots::Slots { items: [(0, empty); heapless_slots::CAP], len: 0 };
        for (lane, o) in outputs.iter().enumerate() {
            let ty = o.ty_or(global_ty);
            let mut u = o.universe;
            for k in 0..o.universes(global_ty) {
                if slots.len >= heapless_slots::CAP {
                    break;
                }
                let (first, n, at) = o.span(global_ty, k);
                let bit = slots.len as u32;
                slots.items[slots.len] = (
                    u,
                    Slot {
                        lane,
                        first_pixel: first,
                        pixels: n,
                        bit,
                        ty,
                        order: o.order.unwrap_or(global_order),
                        reverse: o.reverse,
                        group: usize::from(o.group.max(1)),
                        offset: usize::from(o.offset),
                        leds: o.leds(),
                        dmx_start: at,
                    },
                );
                slots.len += 1;
                u = u.saturating_add(1) & 0x7fff;
            }
        }
        Mapping { slots }
    }

    /// Every universe that should arrive before a frame is complete.
    pub fn all_bits(&self) -> u64 {
        if self.slots.len >= 64 { u64::MAX } else { (1u64 << self.slots.len) - 1 }
    }

    /// The slots fed by `universe` (two outputs may share a universe).
    pub fn slots_for(&self, universe: u16) -> impl Iterator<Item = &Slot> {
        self.slots.iter().filter(move |(u, _)| *u == universe).map(|(_, s)| s)
    }
}

// ── Shared LED memory ───────────────────────────────────────────────────────

/// The outputs as they fit into `budget` bytes per frame copy (all outputs
/// together): each gets what it asks for if it fits; otherwise the space is
/// shared evenly, and what a short output leaves is handed to the others.
/// Only the LED count shrinks (whole LEDs); everything else stays.
pub fn effective(outputs: &[OutputCfg; LANES], global: LedType, budget: usize) -> [OutputCfg; LANES] {
    let mut out = *outputs;
    let want: [usize; LANES] = core::array::from_fn(|k| outputs[k].wire_bytes(global));
    if want.iter().sum::<usize>() <= budget {
        return out;
    }
    let mut order: [usize; LANES] = core::array::from_fn(|k| k);
    order.sort_unstable_by_key(|&k| want[k]);
    let mut left = budget;
    for (i, &k) in order.iter().enumerate() {
        let share = left / (LANES - i);
        let ch = outputs[k].ty_or(global).channels();
        let give = want[k].min(share / ch * ch);
        out[k].leds = (give / ch) as u16;
        left -= give;
    }
    out
}

/// Where each output's bytes sit in one frame copy, and the copy's size
/// (each output starts on a 4-byte boundary).
pub fn layout(lens: &[usize; LANES]) -> ([usize; LANES], usize) {
    let mut at = 0;
    let offsets = core::array::from_fn(|k| {
        let o = at;
        at += lens[k].div_ceil(4) * 4;
        o
    });
    (offsets, at)
}

/// Copies one universe's DMX data into a lane buffer in wire order:
/// colour order, grouping, offset and direction of the slot's output.
/// (Brightness is applied later, on the LED core.)
#[cfg_attr(target_arch = "xtensa", esp_hal::ram)]
pub fn copy_universe(slot: &Slot, dmx: &[u8], lane: &mut [u8]) {
    let dmx = dmx.get(slot.dmx_start..).unwrap_or(&[]);
    let ch = slot.ty.channels();
    if slot.group == 1 && !slot.reverse {
        // The usual case: pixels map 1:1 onto consecutive LEDs.
        let start = (slot.offset + slot.first_pixel) * ch;
        let end = (start + slot.pixels * ch).min(lane.len());
        let Some(dst) = lane.get_mut(start..end) else {
            return;
        };
        let [o0, o1, o2] = slot.order.0.map(usize::from);
        let mut done = 0;
        if ch == 4 {
            for (d, s) in dst.chunks_exact_mut(4).zip(dmx.chunks_exact(4)) {
                d[0] = s[o0];
                d[1] = s[o1];
                d[2] = s[o2];
                d[3] = s[3];
                done += 4;
            }
        } else {
            for (d, s) in dst.chunks_exact_mut(3).zip(dmx.chunks_exact(3)) {
                d[0] = s[o0];
                d[1] = s[o1];
                d[2] = s[o2];
                done += 3;
            }
        }
        // Pixels the packet did not carry go dark.
        dst[done..].fill(0);
        return;
    }
    for i in 0..slot.pixels {
        let src = i * ch;
        let px = |c: usize| dmx.get(src + c).copied().unwrap_or(0);
        let mut bytes = [0u8; 4];
        for (j, &o) in slot.order.0.iter().enumerate() {
            bytes[j] = px(usize::from(o));
        }
        if ch == 4 {
            bytes[3] = px(3);
        }
        let first = slot.offset + (slot.first_pixel + i) * slot.group;
        for led in first..(first + slot.group).min(slot.leds) {
            let led = if slot.reverse { slot.leds - 1 - led } else { led };
            let dst = led * ch;
            if let Some(d) = lane.get_mut(dst..dst + ch) {
                d.copy_from_slice(&bytes[..ch]);
            }
        }
    }
}

/// Brightness table for `percent` (1..=100).
pub fn scale_table(percent: u8) -> [u8; 256] {
    let p = u32::from(percent.clamp(1, 100));
    core::array::from_fn(|v| ((v as u32 * p + 50) / 100) as u8)
}

#[cfg(test)]
mod tests {
    /// The fast path must produce exactly the bytes of the reference path,
    /// for every shape it handles, uneven lanes and odd piece sizes.
    #[test]
    fn fast_path_matches_reference() {
        let mut seed = 0x1234_5678u32;
        let mut rnd = move || {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            seed
        };
        let shapes = [
            Shape { n: 4, h0: 1, h1: 2, sample_rate_hz: 3_333_333 },
            Shape { n: 4, h0: 1, h1: 3, sample_rate_hz: 3_333_333 },
            Shape { n: 8, h0: 2, h1: 5, sample_rate_hz: 6_666_666 },
            Shape { n: 8, h0: 3, h1: 6, sample_rate_hz: 6_666_666 },
        ];
        let lens = [[37usize, 0, 12, 40], [64, 64, 64, 64], [0, 5, 0, 1]];
        for shape in shapes {
            for l in lens {
                let data: [alloc::vec::Vec<u8>; LANES] =
                    core::array::from_fn(|k| (0..l[k]).map(|_| rnd() as u8).collect());
                let lanes: [&[u8]; LANES] = core::array::from_fn(|k| &data[k][..]);
                for piece in [4usize, 36, 64, 1000] {
                    let run = |mut e: Encoder| {
                        let mut out = alloc::vec::Vec::new();
                        let mut buf = alloc::vec![0u8; piece];
                        while !e.done() {
                            let w = e.fill(&mut buf);
                            out.extend_from_slice(&buf[..w]);
                        }
                        out
                    };
                    let fast = run(Encoder::new(lanes, shape, 80));
                    let slow = run(Encoder::new(lanes, shape, 80).slow());
                    assert_eq!(fast, slow, "shape {:?} lens {:?} piece {}", shape, l, piece);
                }
            }
        }
    }

    extern crate std;
    use super::*;

    #[test]
    fn quantize_sk6812_exact() {
        let s = quantize(&Timing::SK6812);
        assert_eq!((s.n, s.h0, s.h1), (4, 1, 2));
        assert_eq!(s.sample_rate_hz, 3_333_333);
        assert_eq!(s.actual_ns(), (300, 600, 1200));
    }

    #[test]
    fn quantize_ws2812_like() {
        let s = quantize(&Timing { t0h_ns: 350, t1h_ns: 700, period_ns: 1250 });
        let (a0, a1, p) = s.actual_ns();
        // Within half a step of 8 samples per bit (78 ns).
        assert!(a0.abs_diff(350) <= 78 && a1.abs_diff(700) <= 78, "{s:?}");
        assert!(SAMPLES_PER_BIT_CHOICES.contains(&s.n));
        assert_eq!(p, 1250);
    }

    #[test]
    fn quantize_fast() {
        let s = quantize(&Timing { t0h_ns: 300, t1h_ns: 600, period_ns: 1000 });
        assert!(s.sample_rate_hz <= MAX_SAMPLE_RATE_HZ);
        assert!(s.h0 < s.h1 && s.h1 < s.n);
        let (a0, a1, _) = s.actual_ns();
        // Within half a step of 8 samples per bit (62 ns).
        assert!(a0.abs_diff(300) <= 62 && a1.abs_diff(600) <= 62, "{s:?}");
        assert!(SAMPLES_PER_BIT_CHOICES.contains(&s.n));
    }

    #[test]
    fn fps() {
        // 512 RGBW at 1.2 µs + 80 µs latch
        assert_eq!(max_fps(512, 4, 1200, 80), 50);
        assert_eq!(max_fps(512, 4, 1250, 80), 48);
    }

    #[test]
    fn encode_roundtrip_all_lanes() {
        let shape = quantize(&Timing::SK6812);
        let l0: std::vec::Vec<u8> = (0..40u8).collect();
        let l1: std::vec::Vec<u8> = (0..40u8).map(|x| x.wrapping_mul(37) ^ 0xA5).collect();
        let l2 = [0xFFu8; 40];
        let l3 = [0x00u8, 0x80, 0x01, 0x55];
        let mut enc = Encoder::new([&l0, &l1, &l2, &l3], shape, 80);
        let total = enc.total_bytes();
        let mut out = std::vec![0u8; total];
        // Fill in pieces that do not line up with byte positions, like the
        // DMA ring does at its end.
        let mut w = 0;
        while !enc.done() {
            let end = (w + 52).min(total);
            w += enc.fill(&mut out[w..end]);
        }
        assert_eq!(w, total);
        assert_eq!(total % 4, 0);
        assert_eq!(decode_lane(&out, shape, 0, 40), l0);
        assert_eq!(decode_lane(&out, shape, 1, 40), l1);
        assert_eq!(decode_lane(&out, shape, 2, 40), l2.to_vec());
        let d3 = decode_lane(&out, shape, 3, 40);
        assert_eq!(&d3[..4], &l3);
        assert!(d3[4..].iter().all(|&b| b == 0));
        // Latch: the tail is all zero, at least 80 µs.
        let latch = total - 40 * 8 * usize::from(shape.n);
        assert!(latch as u64 * 1_000_000 / u64::from(shape.sample_rate_hz) >= 80);
        assert!(out[total - latch..].iter().all(|&b| b == 0));
    }

    #[test]
    fn unused_lanes_stay_low() {
        let shape = quantize(&Timing::SK6812);
        let l0 = [0xFFu8; 4];
        let mut enc = Encoder::new([&l0, &[], &[], &[]], shape, 80);
        let mut out = std::vec![0u8; enc.total_bytes()];
        enc.fill(&mut out);
        assert!(out.iter().all(|&b| b & 0b1110 == 0));
    }

    #[test]
    fn render_identity_blend_gain_dither() {
        let r = Render::IDENTITY;
        assert!(r.is_identity());
        for v in [0u8, 1, 127, 255] {
            assert_eq!(r.byte(0, v, 0, 0), v);
        }
        let half = Render { t: UNITY / 2, ..Render::IDENTITY };
        assert_eq!(half.byte(0, 200, 0, 0), 100);
        let dim = Render { gain: [UNITY / 4; LANES], ..Render::IDENTITY };
        assert_eq!(dim.byte(0, 200, 0, 0), 50);
        // 10 at 10 % brightness = 1.0: dithered, the average over many
        // frames stays near 1 instead of rounding the same way every time.
        let mut sum = 0u32;
        for frame in 0..1000 {
            let d = Render { gain: [UNITY * 3 / 20; LANES], dither: true, frame, ..Render::IDENTITY };
            sum += u32::from(d.byte(0, 10, 0, 7));
        }
        // 10 × 0.15 = 1.5 on average.
        assert!((1300..=1700).contains(&sum), "{sum}");
    }

    #[test]
    fn power_limit_per_output_and_total() {
        // 12 V pixels of 0.72 W at full white (RGBW: 180 mW a channel);
        // 60 W per output (its DC-DC converter), 100 W in total.
        let p = PowerLimit::from_led(720, [4; LANES], 60_000, 100_000);
        assert_eq!(p.mw_per_channel, [180; LANES]);
        // Output 0: 512 LEDs full white = 2048 channels × 180 mW = 368.6 W.
        let full = 2048 * 255;
        let (g, w) = lane_gains(UNITY, &[full, full / 4, 0, 0], &p);
        // Output 0 is cut to its 60 W, then both to the 100 W supply.
        assert!(w[0] <= 60_000, "{w:?}");
        assert!((99_000..=100_000).contains(&w.iter().sum::<u32>()), "{w:?}");
        assert!(g[0] < UNITY / 6);
        assert_eq!(w[2], 0);
        let (_, only_output) = lane_gains(UNITY, &[full, 0, 0, 0], &p);
        assert!(only_output[0] <= 60_000 && only_output[0] > 59_000, "{only_output:?}");
        // RGB outputs split the LED's power over 3 channels.
        assert_eq!(PowerLimit::from_led(720, [3, 4, 3, 4], 0, 0).mw_per_channel, [240, 180, 240, 180]);
        let off = PowerLimit::from_led(720, [4; LANES], 0, 0);
        assert_eq!(lane_gains(UNITY, &[full; LANES], &off).0, [UNITY; LANES]);
    }

    #[test]
    fn encoder_render_matches_rendered_input() {
        // Encoding with a render must equal encoding pre-rendered bytes.
        let shape = quantize(&Timing::SK6812);
        let prev: std::vec::Vec<u8> = (0..64u8).collect();
        let cur: std::vec::Vec<u8> = (0..64u8).map(|x| 255 - x).collect();
        let r = Render { t: 100, gain: [200, 256, 256, 256], dither: true, frame: 3 };
        let pre: std::vec::Vec<u8> = (0..64).map(|i| r.byte(prev[i], cur[i], 0, i)).collect();
        let run = |mut e: Encoder| {
            let mut out = std::vec![0u8; e.total_bytes()];
            e.fill(&mut out);
            out
        };
        let a = run(Encoder::new([&cur, &[], &[], &[]], shape, 80).with_render([&prev, &[], &[], &[]], r));
        let b = run(Encoder::new([&pre, &[], &[], &[]], shape, 80));
        assert_eq!(a, b);
    }

    fn grb() -> Order {
        Order::parse("grb").unwrap()
    }

    #[test]
    fn mapping_rgbw_4x512() {
        let o = |u| OutputCfg::new(512, u);
        let m = Mapping::new(LedType::Rgbw, grb(), &[o(0), o(4), o(8), o(12)]);
        assert_eq!(m.slots.len, 16);
        assert_eq!(m.all_bits(), 0xFFFF_u64);
        let s: std::vec::Vec<_> = m.slots_for(5).copied().collect();
        assert_eq!((s[0].lane, s[0].first_pixel, s[0].pixels, s[0].bit), (1, 128, 128, 5));
    }

    #[test]
    fn mapping_rgb_partial() {
        let off = OutputCfg::new(0, 0);
        let m = Mapping::new(LedType::Rgb, grb(), &[OutputCfg::new(300, 1), off, off, off]);
        // 170 + 130
        assert_eq!(m.slots.len, 2);
        assert_eq!(m.slots_for(2).next().unwrap().pixels, 130);
    }

    #[test]
    fn per_output_type_group_offset_reverse() {
        let mut o = OutputCfg::new(10, 0);
        o.ty = Some(LedType::Rgb);
        o.order = Some(Order::parse("rgb").unwrap());
        o.group = 3;
        o.offset = 1;
        o.reverse = true;
        assert_eq!(o.pixels(), 3); // (10 - 1) / 3
        assert_eq!(o.universes(LedType::Rgbw), 1);
        assert_eq!(o.wire_bytes(LedType::Rgbw), 30);
        let off = OutputCfg::new(0, 0);
        let m = Mapping::new(LedType::Rgbw, grb(), &[o, off, off, off]);
        let slot = *m.slots_for(0).next().unwrap();
        let mut lane = [0u8; 30];
        copy_universe(&slot, &[1, 2, 3, 4, 5, 6, 7, 8, 9], &mut lane);
        // LED 0 dark (offset), pixel 0 -> LEDs 1..4, pixel 1 -> 4..7,
        // pixel 2 -> 7..10; reversed: LED k is written at 9 - k.
        let led = |k: usize| &lane[(9 - k) * 3..(9 - k) * 3 + 3];
        assert_eq!(led(0), &[0, 0, 0]);
        assert_eq!(led(1), &[1, 2, 3]);
        assert_eq!(led(3), &[1, 2, 3]);
        assert_eq!(led(4), &[4, 5, 6]);
        assert_eq!(led(9), &[7, 8, 9]);
    }

    /// Interpolation with uneven lanes (60 and 512 RGBW LEDs) and the odd
    /// piece sizes of ring top-ups must give exactly the rendered bytes,
    /// encoded in one go.
    #[test]
    fn render_pieces_match_reference() {
        let mut seed = 0x9876_5431u32;
        let mut rnd = move || {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            seed
        };
        let shape = Shape { n: 4, h0: 1, h1: 3, sample_rate_hz: 4_188_481 };
        let lens = [60 * 4, 512 * 4, 512 * 4, 512 * 4];
        for k in [0usize, 5, 30, 58] {
            let mk = |at: usize| -> [alloc::vec::Vec<u8>; LANES] {
                core::array::from_fn(|l| {
                    let mut v = alloc::vec![0u8; lens[l]];
                    v[at * 4..at * 4 + 4].copy_from_slice(&[255; 4]);
                    v
                })
            };
            let cur = mk(k + 1);
            let prev = mk(k);
            for t in [1u16, 100, 200, 255] {
                let r = Render { t, gain: [UNITY; LANES], dither: false, frame: 7 };
                let rendered: [alloc::vec::Vec<u8>; LANES] = core::array::from_fn(|l| {
                    (0..lens[l]).map(|i| r.byte(prev[l][i], cur[l][i], l, i)).collect()
                });
                let reference = {
                    let lanes: [&[u8]; LANES] = core::array::from_fn(|l| &rendered[l][..]);
                    let mut e = Encoder::new(lanes, shape, 100);
                    let mut out = alloc::vec::Vec::new();
                    let mut buf = alloc::vec![0u8; 1 << 20];
                    while !e.done() {
                        let w = e.fill(&mut buf);
                        out.extend_from_slice(&buf[..w]);
                    }
                    out
                };
                for _ in 0..3 {
                    let lanes: [&[u8]; LANES] = core::array::from_fn(|l| &cur[l][..]);
                    let prevs: [&[u8]; LANES] = core::array::from_fn(|l| &prev[l][..]);
                    let mut e = Encoder::new(lanes, shape, 100).with_render(prevs, r);
                    let mut out = alloc::vec::Vec::new();
                    while !e.done() {
                        let piece = 1 + (rnd() as usize % 3000);
                        let mut buf = alloc::vec![0u8; piece];
                        let w = e.fill(&mut buf);
                        out.extend_from_slice(&buf[..w]);
                    }
                    assert_eq!(out.len(), reference.len(), "k {k} t {t}");
                    assert!(out == reference, "k {k} t {t}: first difference at {:?}", out.iter().zip(&reference).position(|(a, b)| a != b));
                }
            }
        }
    }

    #[test]
    fn shared_memory_cap() {
        let o = |n| OutputCfg::new(n, 0);
        // Fits: unchanged.
        let all = [o(512), o(512), o(512), o(512)];
        assert_eq!(effective(&all, LedType::Rgbw, 8192), all);
        // Too big: even shares of 1024 bytes (256 RGBW LEDs) each.
        let e = effective(&all, LedType::Rgbw, 4096);
        assert!(e.iter().all(|x| x.leds == 256));
        // A short output's leftover goes to the others.
        let e = effective(&[o(100), o(2000), o(2000), o(0)], LedType::Rgbw, 8000);
        assert_eq!(e[0].leds, 100);
        assert_eq!(e[3].leds, 0);
        assert_eq!((e[1].leds, e[2].leds), (950, 950)); // (8000 - 400) / 2 / 4
        let total: usize = e.iter().map(|x| x.wire_bytes(LedType::Rgbw)).sum();
        assert!(total <= 8000);
        // Layout: 4-byte aligned offsets.
        let (off, stride) = layout(&[3, 8, 0, 5]);
        assert_eq!(off, [0, 4, 12, 12]);
        assert_eq!(stride, 20);
        // More than 32 universes: the mask is 64 bits wide.
        let big = [o(1024), o(1024), o(1024), o(1024)];
        let m = Mapping::new(LedType::Rgbw, grb(), &big);
        assert_eq!(m.slots.len, 32);
        let big = [o(1200), o(1200), o(1200), o(1200)];
        let m = Mapping::new(LedType::Rgbw, grb(), &big);
        assert_eq!(m.slots.len, 40); // 10 universes each
        assert_eq!(m.all_bits(), (1u64 << 40) - 1);
    }

    #[test]
    fn dmx_start_address() {
        let off = OutputCfg::new(0, 0);
        let mut o = OutputCfg::new(10, 3);
        o.ty = Some(LedType::Rgb);
        o.order = Some(Order::parse("rgb").unwrap());
        o.start = 501; // byte 500: 4 pixels fit in the first universe
        assert_eq!(o.universes(LedType::Rgbw), 2);
        assert_eq!(o.span(LedType::Rgbw, 0), (0, 4, 500));
        assert_eq!(o.span(LedType::Rgbw, 1), (4, 6, 0));
        let m = Mapping::new(LedType::Rgbw, grb(), &[o, off, off, off]);
        assert_eq!(m.slots.len, 2);
        let s0 = *m.slots_for(3).next().unwrap();
        let s1 = *m.slots_for(4).next().unwrap();
        assert_eq!((s0.first_pixel, s0.pixels, s0.dmx_start), (0, 4, 500));
        assert_eq!((s1.first_pixel, s1.pixels, s1.dmx_start), (4, 6, 0));
        let mut dmx = [0u8; 512];
        dmx[500..503].copy_from_slice(&[1, 2, 3]);
        dmx[509..512].copy_from_slice(&[7, 8, 9]);
        let mut lane = [0u8; 30];
        copy_universe(&s0, &dmx, &mut lane);
        assert_eq!(&lane[..3], &[1, 2, 3]);
        assert_eq!(&lane[9..12], &[7, 8, 9]);
        copy_universe(&s1, &[4, 5, 6], &mut lane);
        assert_eq!(&lane[12..15], &[4, 5, 6]);
        // Too high for one pixel: clamped so one whole pixel still fits.
        let mut w = OutputCfg::new(1, 0);
        w.start = 512;
        assert_eq!(w.start_byte(LedType::Rgbw), 508);
        assert_eq!(w.universes(LedType::Rgbw), 1);
        // Default start: unchanged layout.
        assert_eq!(OutputCfg::new(300, 0).universes(LedType::Rgb), 2);
    }

    #[test]
    fn copy_fast_path_short_packet_and_rgb() {
        let off = OutputCfg::new(0, 0);
        let mut o = OutputCfg::new(6, 0);
        o.ty = Some(LedType::Rgb);
        o.offset = 1;
        let m = Mapping::new(LedType::Rgbw, grb(), &[o, off, off, off]);
        let slot = *m.slots_for(0).next().unwrap();
        let mut lane = [9u8; 18];
        // Only two of five pixels carried; GRB order; LED 0 is the offset.
        copy_universe(&slot, &[10, 20, 30, 40, 50, 60], &mut lane);
        assert_eq!(&lane[..3], &[9, 9, 9]);
        assert_eq!(&lane[3..9], &[20, 10, 30, 50, 40, 60]);
        assert_eq!(&lane[9..], &[0; 9]);
    }

    #[test]
    fn copy_order_and_brightness() {
        let slot = Slot {
            lane: 0,
            first_pixel: 1,
            pixels: 2,
            bit: 0,
            ty: LedType::Rgbw,
            order: grb(),
            reverse: false,
            group: 1,
            offset: 0,
            leds: 3,
            dmx_start: 0,
        };
        let mut lane = [0u8; 12];
        let dmx = [10, 20, 30, 40, 50, 60, 70, 80];
        copy_universe(&slot, &dmx, &mut lane);
        assert_eq!(lane, [0, 0, 0, 0, 20, 10, 30, 40, 60, 50, 70, 80]);
        let half = scale_table(50);
        assert_eq!(half[255], 128);
        assert_eq!(half[0], 0);
        assert!(Order::parse("rgg").is_none());
        assert_eq!(&Order::parse("BRG").unwrap().name(), b"brg");
    }
}
