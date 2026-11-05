//! Audio analysis for the microphone: level, three bands, onsets, beat and
//! beat phase, activity, clipping and mic faults. The firmware feeds it the
//! decimated mic samples and sends each [`Snapshot`] as a DMX universe
//! ([`encode`], channel map in docs/api.md).
//!
//! Integer arithmetic only (bit-exact on the host and the chip). Samples are
//! signed 24-bit (the mic's data in the top bits of the I2S word, shifted
//! down by 8) at [`RATE`]; one snapshot comes out every [`FRAME`] samples.

/// Analysis rate (the I2S rate decimated by 3).
pub const RATE: u32 = 16_000;
/// Snapshots per second (one DMX frame each).
pub const FRAME_HZ: u32 = 44;
/// Samples per snapshot.
pub const FRAME: u32 = RATE / FRAME_HZ;
/// Channels in the audio universe ([`encode`]).
pub const CHANNELS: usize = 16;

/// Full scale of a 24-bit sample.
const FS: i32 = 1 << 23;
/// Envelopes and onsets are evaluated every this many samples (4 ms).
const BLOCK: u32 = 64;
/// dB in 1/256 dB.
type Db = i32;
const DB: Db = 256;

/// The settings the analysis takes (`Settings::audio` in the firmware).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Params {
    /// Automatic gain: each value follows its own recent peak.
    pub agc: bool,
    /// Gain without AGC, dB (-20..=40).
    pub gain_db: i8,
    /// Below this input level (dBFS) everything reads 0 (-90..=-20).
    pub gate_db: i8,
    /// Level and band attack and release, ms.
    pub attack_ms: u16,
    pub release_ms: u16,
    /// Onset and beat sensitivity, 1 (only strong hits) ..= 10.
    pub sensitivity: u8,
    /// Per band (low, mid, high): output × this / 128 (128 = ×1). Only the
    /// control channels set it.
    pub band_gain: [u8; 3],
    /// Crossovers: low/mid (40..=500 Hz) and mid/high (1000..=8000 Hz).
    pub low_hz: u16,
    pub high_hz: u16,
    /// The window the values are shown in, dB below the top (12..=48): the
    /// smaller, the more each hit swings a value.
    pub range_db: u8,
}

impl Default for Params {
    fn default() -> Self {
        Params { agc: true, gain_db: 0, gate_db: -70, attack_ms: 10, release_ms: 100, sensitivity: 5, band_gain: [128; 3], low_hz: 150, high_hz: 2000, range_db: 24 }
    }
}

/// Channels of the control fixture (console → node, docs/api.md).
pub const CONTROL_CHANNELS: usize = 11;

/// What the control channels ask for besides parameters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Control {
    /// Hold the output values.
    pub freeze: bool,
    /// The tap channel is up (a beat on each rising edge).
    pub tap: bool,
}

/// `p` with the control channels' overrides: a channel at 0 keeps the
/// settings page's value.
pub fn apply_control(p: &Params, c: &[u8; CONTROL_CHANNELS]) -> (Params, Control) {
    let mut q = *p;
    // 1..=255 onto lo..=hi.
    let span = |v: u8, lo: i32, hi: i32| lo + (i32::from(v) - 1) * (hi - lo) / 254;
    if c[0] != 0 {
        q.agc = c[0] >= 128;
    }
    if c[1] != 0 {
        q.gain_db = span(c[1], -20, 40) as i8;
    }
    if c[2] != 0 {
        q.gate_db = span(c[2], -90, -20) as i8;
    }
    if c[3] != 0 {
        q.attack_ms = span(c[3], 1, 500) as u16;
    }
    if c[4] != 0 {
        q.release_ms = span(c[4], 10, 3000) as u16;
    }
    if c[5] != 0 {
        q.sensitivity = span(c[5], 1, 10) as u8;
    }
    for k in 0..3 {
        if c[6 + k] != 0 {
            q.band_gain[k] = c[6 + k];
        }
    }
    (q, Control { freeze: c[9] >= 128, tap: c[10] >= 128 })
}

/// What the room sounds like.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Activity {
    /// Below the gate.
    Silence,
    /// Sound, but no steady beat.
    Noise,
    /// A steady beat.
    Music,
}

/// State of the mic and the analysis.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// No samples or stuck samples: values read 0.
    Fault,
    /// Below the gate.
    Gated,
    Active,
    /// The input reached full scale in the last 300 ms.
    Clipping,
}

/// One frame of results.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Snapshot {
    /// Level after gain, 0..=65535.
    pub level: u16,
    /// Level peak hold, 0..=255 (falls over 1 s).
    pub peak: u8,
    /// Beat pulse (held 80 ms).
    pub beat: bool,
    /// Position within the current beat, 0..=255 (0 without a tempo).
    pub phase: u8,
    /// Low (< 150 Hz), mid, high (> 2 kHz), 0..=255.
    pub bands: [u8; 3],
    /// Onset pulses per band (held 80 ms).
    pub onsets: [bool; 3],
    pub activity: Activity,
    /// Input level, dBFS (before gain).
    pub input_db: i16,
    /// Beat period in ms (0: none yet), for the web page.
    pub beat_ms: u16,
    /// The gains in effect, dB (the AGC's, or the manual one): level, low,
    /// mid, high.
    pub gain_db: [i8; 4],
    pub status: Status,
}

impl Snapshot {
    /// All zero with `status` (mic fault, analysis off).
    pub const fn empty(status: Status) -> Self {
        Snapshot {
            level: 0,
            peak: 0,
            beat: false,
            phase: 0,
            bands: [0; 3],
            onsets: [false; 3],
            activity: Activity::Silence,
            input_db: -120,
            beat_ms: 0,
            gain_db: [0; 4],
            status,
        }
    }
}

/// The audio universe: channel map in docs/api.md.
pub fn encode(s: &Snapshot) -> [u8; CHANNELS] {
    let mut c = [0u8; CHANNELS];
    c[0..2].copy_from_slice(&s.level.to_be_bytes());
    c[2] = s.peak;
    c[3] = if s.beat { 255 } else { 0 };
    c[4] = s.phase;
    c[5..8].copy_from_slice(&s.bands);
    for (k, &o) in s.onsets.iter().enumerate() {
        c[8 + k] = if o { 255 } else { 0 };
    }
    c[11] = match s.activity {
        Activity::Silence => 0,
        Activity::Noise => 128,
        Activity::Music => 255,
    };
    // 0 = -96 dBFS or less, 255 = 0 dBFS.
    c[12] = ((i32::from(s.input_db) + 96).clamp(0, 96) * 255 / 96) as u8;
    c[13] = match s.status {
        Status::Fault => 64,
        Status::Gated => 128,
        Status::Active => 192,
        Status::Clipping => 255,
    };
    c
}

/// One-pole low-pass coefficient (Q16) for a cut-off of `hz`:
/// 1 - e^(-2π·hz/RATE), with e^-w as (1 - w/256)^256.
pub fn coef_hz(hz: u32) -> i64 {
    // w = 2π · hz / RATE in Q32 (2π · 2^32 = 26 986 075 409).
    let w = 26_986_075_409i64 * i64::from(hz) / i64::from(RATE);
    let mut e = i128::from((1i64 << 32) - (w >> 8));
    for _ in 0..8 {
        e = e * e >> 32; // squaring in Q32 (only when a crossover changes)
    }
    ((((1i128 << 32) - e) >> 16) as i64).clamp(1, 65535)
}

/// One-pole filter coefficient (Q16) for a time constant of `ms`.
const fn coef_ms(ms: u32) -> i64 {
    let n = ms * RATE / 1000;
    let k = 65536 / (if n == 0 { 1 } else { n }) as i64;
    if k > 65535 { 65535 } else if k < 1 { 1 } else { k }
}

/// The fixed time constants: band and onset rise (2 ms), onset fall
/// (40 ms).
const K2MS: i64 = coef_ms(2);
const K40MS: i64 = coef_ms(40);

/// One-pole low-pass: `y += (x - y) * k`, `k` in Q16.
#[derive(Debug, Clone, Copy, Default)]
struct OnePole(i32);

impl OnePole {
    fn step(&mut self, x: i32, k: i64) -> i32 {
        self.0 += ((i64::from(x) - i64::from(self.0)) * k >> 16) as i32;
        self.0
    }
}

/// Envelope follower on |x| with separate attack and release (Q16 each).
#[derive(Debug, Clone, Copy, Default)]
struct Env(i32);

impl Env {
    fn step(&mut self, x: i32, attack: i64, release: i64) -> i32 {
        let v = x.saturating_abs();
        let k = if v > self.0 { attack } else { release };
        self.0 += ((i64::from(v) - i64::from(self.0)) * k >> 16) as i32;
        self.0
    }
}

/// 20·log10(v / FS) in 1/256 dB; -120 dB for 0.
fn db(v: i32) -> Db {
    if v <= 0 {
        return -120 * DB;
    }
    let v = v as u32;
    let msb = 31 - v.leading_zeros() as i32;
    // Fraction of the mantissa, Q16, and log2(1+f) ≈ f + 0.34·f·(1-f).
    let f = if msb >= 16 { (v >> (msb - 16)) as i64 - 65536 } else { ((v << (16 - msb)) as i64) - 65536 };
    let frac = f + (f * (65536 - f) >> 16) * 22282 / 65536;
    let log2_q16 = (i64::from(msb - 23) << 16) + frac;
    // 20·log10(2) = 6.0206 dB per octave.
    ((log2_q16 * 1541) >> 16) as Db
}

/// 0..=1 (Q16) for `x` in the window [top - range, top] (1/256 dB).
fn window(x: Db, top: Db, range: Db) -> u32 {
    (((x - (top - range)).clamp(0, range) as i64 * 65535) / i64::from(range)) as u32
}

// Values are shown in a window of `Params::range_db` below 0 dBFS, after the
// gain; the AGC puts the average at 60 % of it (40 % of the range below the
// top), so peaks have that much headroom.
/// The AGC's range, as the manual gain's.
const AGC_MIN: Db = -20 * DB;
const AGC_MAX: Db = 40 * DB;
/// Per block (4 ms): ~10 s time constant (65536 · 4 ms / 10 s ≈ 26).
const AGC_K: i64 = 26;

/// A slow gain stage for one value: follows its average level over about
/// 10 s and puts it at AGC_TARGET. It holds while gated, and for input 20 dB
/// below its average (decays, breaks) unless that lasts 5 s (the music was
/// turned down): pauses do not turn it up.
#[derive(Debug, Clone, Copy)]
struct SlowAgc {
    /// Average level (1/65536 dB); `None` until there was sound.
    avg: Option<i64>,
    /// Blocks in a row 20 dB or more below the average.
    low: u32,
}

impl SlowAgc {
    const fn new() -> Self {
        SlowAgc { avg: None, low: 0 }
    }

    fn step(&mut self, d: Db, gated: bool) {
        if gated {
            return;
        }
        let x = i64::from(d) << 8;
        let avg = self.avg.get_or_insert(x);
        let low = x < *avg - (i64::from(20 * DB) << 8);
        self.low = if low { self.low.saturating_add(1) } else { 0 };
        if !low || self.low > 5 * RATE / BLOCK {
            *avg += (x - *avg) * AGC_K >> 16;
        }
    }

    fn gain(&self, target: Db) -> Db {
        self.avg.map_or(0, |a| (target - (a >> 8) as Db).clamp(AGC_MIN, AGC_MAX))
    }
}

/// Onset detection on one band: the fast envelope jumping above the slow one.
#[derive(Debug, Clone, Copy)]
struct Onset {
    fast: Env,
    slow: OnePole,
    armed: bool,
    last: u32,
    lockout: u32,
}

impl Onset {
    const fn new(lockout_ms: u32) -> Self {
        Onset { fast: Env(0), slow: OnePole(0), armed: true, last: 0, lockout: lockout_ms * RATE / 1000 }
    }
}

/// Samples an onset or beat pulse is held (80 ms: at least 3 DMX frames).
const PULSE: u32 = 80 * RATE / 1000;

/// The analysis state. About 200 bytes.
pub struct Analyzer {
    now: u32,
    dc: i64,
    /// Two-pole filters (12 dB/octave): low-pass 150 Hz, high-pass 150 Hz,
    /// low-pass 2 kHz (mid), high-pass 2 kHz.
    lp150: [OnePole; 2],
    /// The crossovers in use and their coefficients.
    cross_hz: (u16, u16),
    cross_k: (i64, i64),
    /// Attack and release in use (ms) and their coefficients.
    env_ms: (u16, u16),
    env_k: (i64, i64),
    hp150: [OnePole; 2],
    lp2k: [OnePole; 2],
    hp2k: [OnePole; 2],
    level: Env,
    bands: [Env; 3],
    /// AGC per value: level, low, mid, high (each band uses its range).
    agc: [SlowAgc; 4],
    peak: i32,
    onsets: [Onset; 3],
    // Beat tracking (in samples).
    last_beat: Option<u32>,
    period: u32,
    misfits: u8,
    steady: u8,
    clip_until: u32,
    gated_since: Option<u32>,
    // Fault: the input does not move.
    win_min: i32,
    win_max: i32,
    win_start: u32,
    stuck_windows: u8,
    out: Snapshot,
}

impl Default for Analyzer {
    fn default() -> Self {
        Self::new()
    }
}

impl Analyzer {
    pub const fn new() -> Self {
        Analyzer {
            now: 0,
            dc: 0,
            lp150: [OnePole(0); 2],
            cross_hz: (0, 0),
            cross_k: (0, 0),
            env_ms: (0, 0),
            env_k: (0, 0),
            hp150: [OnePole(0); 2],
            lp2k: [OnePole(0); 2],
            hp2k: [OnePole(0); 2],
            level: Env(0),
            bands: [Env(0); 3],
            agc: [SlowAgc::new(); 4],
            peak: 0,
            onsets: [Onset::new(250), Onset::new(120), Onset::new(80)],
            last_beat: None,
            period: 0,
            misfits: 0,
            steady: 0,
            clip_until: 0,
            gated_since: Some(0),
            win_min: i32::MAX,
            win_max: i32::MIN,
            win_start: 0,
            stuck_windows: 0,
            out: Snapshot::empty(Status::Gated),
        }
    }

    /// Feeds one sample; every [`FRAME`] samples returns the snapshot.
    pub fn push(&mut self, raw: i32, p: &Params) -> Option<Snapshot> {
        self.now = self.now.wrapping_add(1);
        let now = self.now;

        // Stuck input (all samples equal for 2 s).
        self.win_min = self.win_min.min(raw);
        self.win_max = self.win_max.max(raw);
        if now.wrapping_sub(self.win_start) >= RATE {
            self.stuck_windows = if self.win_max == self.win_min { self.stuck_windows.saturating_add(1) } else { 0 };
            self.win_min = i32::MAX;
            self.win_max = i32::MIN;
            self.win_start = now;
        }
        if raw.saturating_abs() >= FS - FS / 100 {
            self.clip_until = now.wrapping_add(300 * RATE / 1000);
        }

        // DC and drift removal: a 20 Hz high-pass (the SPH0645's offset
        // wanders by several % of full scale within a second).
        let x16 = i64::from(raw) << 16;
        self.dc += (x16 - self.dc) >> 7;
        let x = ((x16 - self.dc) >> 16) as i32;

        // Bands: low < 150 Hz, mid 150 Hz..2 kHz, high > 2 kHz.
        if (p.low_hz, p.high_hz) != self.cross_hz {
            self.cross_hz = (p.low_hz, p.high_hz);
            self.cross_k = (coef_hz(u32::from(p.low_hz)), coef_hz(u32::from(p.high_hz)));
        }
        let (kl, kh) = self.cross_k;
        let low1 = self.lp150[0].step(x, kl);
        let low = self.lp150[1].step(low1, kl);
        let h1 = x - self.hp150[0].step(x, kl);
        let h2 = h1 - self.hp150[1].step(h1, kl);
        let m1 = self.lp2k[0].step(h2, kh);
        let mid = self.lp2k[1].step(m1, kh);
        let g1 = x - self.hp2k[0].step(x, kh);
        let high = g1 - self.hp2k[1].step(g1, kh);
        let band = [low, mid, high];

        if (p.attack_ms, p.release_ms) != self.env_ms {
            self.env_ms = (p.attack_ms, p.release_ms);
            self.env_k = (coef_ms(u32::from(p.attack_ms)), coef_ms(u32::from(p.release_ms)));
        }
        let (atk, rel) = self.env_k;
        let level = self.level.step(x, atk, rel);
        // Bands rise at once (2 ms) to show each hit; they fall at the
        // release.
        let mut env = [0i32; 3];
        for k in 0..3 {
            env[k] = self.bands[k].step(band[k], K2MS, rel);
            let o = &mut self.onsets[k];
            o.fast.step(band[k], K2MS, K40MS);
        }

        if now % BLOCK == 0 {
            self.block(level, env, p);
        }
        (now % FRAME == 0).then_some(self.out)
    }

    /// Per block: dB values, gate, AGC, onsets, beat, the snapshot.
    fn block(&mut self, level: i32, env: [i32; 3], p: &Params) {
        let now = self.now;
        let input = db(level);
        let gate = i32::from(p.gate_db) * DB;
        let gated = input < gate;
        if gated {
            self.gated_since.get_or_insert(now);
        } else {
            self.gated_since = None;
        }

        // Gains: the AGC's per value (slow, see SlowAgc) or the manual one.
        let range = i32::from(p.range_db.clamp(12, 48)) * DB;
        let levels = [input, db(env[0]), db(env[1]), db(env[2])];
        let mut gains = [i32::from(p.gain_db) * DB; 4];
        // A band below the gate on its own is noise too (e.g. the high band
        // with bass-only music): its AGC holds, it reads 0.
        let band_gated: [bool; 4] = core::array::from_fn(|k| gated || levels[k] < gate);
        for k in 0..4 {
            self.agc[k].step(levels[k], band_gated[k]);
            if p.agc {
                gains[k] = self.agc[k].gain(-range * 2 / 5);
            }
        }
        // Values 0..1 (Q16) in the window below 0 dBFS after the gain.
        let value = |k: usize| -> u32 { if band_gated[k] { 0 } else { window(levels[k] + gains[k], 0, range) } };
        let lvl = value(0);
        let mut bands = [0u8; 3];
        for k in 0..3 {
            let v = (value(k + 1) >> 8) * u32::from(p.band_gain[k]) / 128;
            bands[k] = v.min(255) as u8;
        }

        // Onsets: fast envelope this many dB above the slow one.
        let threshold = (12 - i32::from(p.sensitivity.clamp(1, 10))) * DB;
        let mut onsets = [false; 3];
        let mut kick = false;
        for k in 0..3 {
            let o = &mut self.onsets[k];
            let fast = db(o.fast.0);
            let slow = db(o.slow.step(o.fast.0, coef_ms(600) * i64::from(BLOCK)));
            let rise = fast - slow;
            if !o.armed && rise < threshold / 2 {
                o.armed = true;
            }
            if o.armed && !gated && fast > gate && rise > threshold && now.wrapping_sub(o.last) >= o.lockout {
                o.armed = false;
                o.last = now;
                if k == 0 {
                    kick = true;
                }
            }
            onsets[k] = o.last != 0 && now.wrapping_sub(o.last) < PULSE;
        }
        if kick {
            self.beat(now);
        }

        // Beat phase: free-running at the tempo for up to 4 beats.
        let mut phase = 0u8;
        let mut beat = false;
        if let Some(last) = self.last_beat {
            let since = now.wrapping_sub(last);
            beat = since < PULSE;
            if self.period > 0 && since < 4 * self.period {
                phase = ((since % self.period) * 256 / self.period) as u8;
            }
            if since > 2 * RATE {
                self.steady = 0;
                self.period = 0;
            }
        }

        let silent_for = self.gated_since.map_or(0, |t| now.wrapping_sub(t));
        let activity = if silent_for > RATE / 2 {
            Activity::Silence
        } else if self.steady >= 3 {
            Activity::Music
        } else {
            Activity::Noise
        };

        // Peak hold: falls from full scale to 0 in 1 s.
        let lvl16 = lvl as i32;
        self.peak = (self.peak - 65535 * BLOCK as i32 / RATE as i32).max(lvl16);

        let status = if self.stuck_windows >= 2 {
            Status::Fault
        } else if now.wrapping_sub(self.clip_until) > u32::MAX / 2 {
            Status::Clipping
        } else if gated {
            Status::Gated
        } else {
            Status::Active
        };
        self.out = if status == Status::Fault {
            Snapshot::empty(Status::Fault)
        } else {
            Snapshot {
                level: lvl.min(65535) as u16,
                peak: (self.peak >> 8).clamp(0, 255) as u8,
                beat,
                phase,
                bands,
                onsets,
                activity,
                input_db: input.div_euclid(DB).clamp(-120, 0) as i16,
                beat_ms: (self.period * 1000 / RATE) as u16,
                gain_db: gains.map(|g| g.div_euclid(DB) as i8),
                status,
            }
        };
    }

    /// A beat by hand (the tap channel): a pulse now, and the tempo follows
    /// the taps like it follows kicks.
    pub fn tap(&mut self) {
        let now = self.now;
        self.onsets[0].last = now;
        self.beat(now);
    }

    /// A kick: updates the beat period (60..=200 BPM).
    fn beat(&mut self, now: u32) {
        const MIN: u32 = 300 * RATE / 1000;
        const MAX: u32 = 1000 * RATE / 1000;
        if let Some(last) = self.last_beat {
            let ibi = now.wrapping_sub(last);
            if (MIN..=MAX).contains(&ibi) {
                let p = self.period;
                if p == 0 {
                    self.period = ibi;
                } else if ibi.abs_diff(p) < p / 5 {
                    self.period = (3 * p + ibi) / 4;
                    self.misfits = 0;
                    self.steady = self.steady.saturating_add(1);
                } else {
                    // Two off-tempo beats in a row: a new tempo.
                    self.misfits += 1;
                    if self.misfits >= 2 {
                        self.period = ibi;
                        self.misfits = 0;
                        self.steady = 0;
                    }
                }
            } else if ibi > MAX {
                self.steady = 0;
            }
        }
        self.last_beat = Some(now);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    extern crate std;
    use std::vec::Vec;

    /// Integer sine, amplitude `a`, frequency `f` Hz, at sample `n`.
    fn sine(a: i32, f: u32, n: u32) -> i32 {
        // Bhaskara approximation of sin over one period, good to ~0.2 %.
        let t = (u64::from(n) * u64::from(f) * 360 / u64::from(RATE) % 360) as i64;
        let (t, sign) = if t < 180 { (t, 1) } else { (t - 180, -1) };
        let s = 4 * t * (180 - t) * 65536 / (40500 - t * (180 - t));
        (i64::from(a) * s * sign >> 16) as i32
    }

    fn run(p: &Params, secs: u32, mut f: impl FnMut(u32) -> i32) -> Vec<Snapshot> {
        let mut a = Analyzer::new();
        (0..secs * RATE).filter_map(|n| a.push(f(n), p)).collect()
    }

    #[test]
    fn db_is_close() {
        assert!((db(FS) - 0).abs() < DB / 4);
        assert!((db(FS / 2) + 6 * DB).abs() < DB / 4);
        assert!((db(FS / 1000) + 60 * DB).abs() < DB / 2);
        assert_eq!(db(0), -120 * DB);
    }

    #[test]
    fn crossover_coefficients() {
        // The fixed values before: 150 Hz -> 3749, 2 kHz -> 35660.
        assert!(coef_hz(150).abs_diff(3749) < 40, "{}", coef_hz(150));
        assert!(coef_hz(2000).abs_diff(35660) < 200, "{}", coef_hz(2000));
        assert!(coef_hz(40) < coef_hz(500) && coef_hz(8000) < 65535);
    }

    #[test]
    fn crossover_moves_the_bands() {
        // 300 Hz: mid with the default, low with the low band up to 500 Hz.
        let p = Params { agc: false, ..Params::default() };
        let wide = Params { low_hz: 500, ..p };
        let a = *run(&p, 1, |n| sine(FS / 8, 300, n)).last().unwrap();
        let b = *run(&wide, 1, |n| sine(FS / 8, 300, n)).last().unwrap();
        assert!(a.bands[1] > a.bands[0] && b.bands[0] > b.bands[1], "{:?} {:?}", a.bands, b.bands);
    }

    #[test]
    fn frames_at_44_hz() {
        assert_eq!(run(&Params::default(), 1, |_| 0).len(), 44);
    }

    #[test]
    fn silence_is_gated() {
        let s = run(&Params::default(), 2, |n| (n % 3) as i32); // a few LSB of noise
        let last = s.last().unwrap();
        assert_eq!(last.status, Status::Gated);
        assert_eq!(last.activity, Activity::Silence);
        assert_eq!((last.level, last.bands), (0, [0; 3]));
    }

    #[test]
    fn bands_split_by_frequency() {
        // -18 dBFS without AGC: about 100 of 255 in its own band (30 dB window).
        let p = Params { agc: false, range_db: 30, ..Params::default() };
        let last = |f: u32| *run(&p, 1, |n| sine(FS / 8, f, n)).last().unwrap();
        let (low, mid, high) = (last(60), last(600), last(6000));
        assert!(low.bands[0] > 80 && low.bands[1] + 40 < low.bands[0] && low.bands[2] < 10, "60 Hz: {:?}", low.bands);
        assert!(mid.bands[1] > 60 && mid.bands[0] + 20 < mid.bands[1] && mid.bands[2] + 20 < mid.bands[1], "600 Hz: {:?}", mid.bands);
        assert!(high.bands[2] > 20 && high.bands[1] + 15 < high.bands[2] && high.bands[0] < 10, "6 kHz: {:?}", high.bands);
        assert!(low.level > 20000 && low.status == Status::Active);
    }

    #[test]
    fn kicks_give_beats_and_tempo() {
        // 120 BPM: a 60 Hz burst of 80 ms every 500 ms over quiet noise.
        let s = run(&Params::default(), 8, |n| {
            let t = n % (RATE / 2);
            let noise = ((n.wrapping_mul(2654435761) >> 20) as i32 & 0xfff) - 0x800;
            if t < 80 * RATE / 1000 { sine(FS / 4, 60, n) + noise } else { noise }
        });
        let beats = s.windows(2).filter(|w| w[1].beat && !w[0].beat).count();
        assert!((14..=17).contains(&beats), "{beats} beats in 8 s");
        let last = s.last().unwrap();
        assert!(last.beat_ms.abs_diff(500) < 25, "period {} ms", last.beat_ms);
        assert_eq!(last.activity, Activity::Music);
        // The phase rises within a beat.
        let ramp: Vec<u8> = s[s.len() - 22..].iter().map(|f| f.phase).collect();
        assert!(ramp.windows(2).filter(|w| w[1] > w[0]).count() >= 15, "{ramp:?}");
    }

    #[test]
    fn agc_is_slow_and_holds_in_silence() {
        let p = Params::default();
        let mut a = Analyzer::new();
        let mut last = Snapshot::empty(Status::Gated);
        let mut feed = |a: &mut Analyzer, secs: u32, amp: i32, off: u32| {
            for n in 0..secs * RATE {
                if let Some(s) = a.push(sine(amp, 600, n + off), &p) {
                    last = s;
                }
            }
            last
        };
        // -38 dBFS for 40 s: the average goes to about 60 % of the range.
        let quiet = feed(&mut a, 40, FS / 80, 0);
        assert!((20..=32).contains(&quiet.gain_db[0]), "gain {}", quiet.gain_db[0]);
        assert!((30000..=48000).contains(&quiet.level), "level {}", quiet.level);
        // 20 dB louder: the level jumps up at once (the gain does not follow
        // within a second) ...
        let loud = feed(&mut a, 1, FS / 8, 0);
        assert!(loud.level > 60000 && loud.gain_db[0] >= quiet.gain_db[0] - 3, "{} {}", loud.level, loud.gain_db[0]);
        // ... and is back near the target after ~40 s.
        let settled = feed(&mut a, 40, FS / 8, 0);
        assert!((30000..=48000).contains(&settled.level), "level {}", settled.level);
        assert!(settled.gain_db[0] < quiet.gain_db[0] - 15);
        // Silence (below the gate; not exact zeros, those are a stuck mic)
        // for 20 s does not turn it up.
        let g = settled.gain_db[0];
        feed(&mut a, 20, 80, 0);
        let after = feed(&mut a, 1, FS / 8, 0);
        assert!(after.gain_db[0].abs_diff(g) <= 1, "{} vs {}", after.gain_db[0], g);
        // Turned down by 26 dB for good: after the 5 s hold it follows.
        let down = feed(&mut a, 60, FS / 160, 0);
        assert!(down.gain_db[0] > g + 15, "{} vs {}", down.gain_db[0], g);
    }

    #[test]
    fn bands_have_their_own_agc() {
        // A loud low tone and a 24 dB quieter high one: with one gain the high
        // band would sit below the window; with its own AGC it uses its range.
        let p = Params::default();
        let s = *run(&p, 40, |n| sine(FS / 8, 60, n) + sine(FS / 128, 5000, n)).last().unwrap();
        assert!(s.bands[0] > 100 && s.bands[2] > 100, "{:?} gains {:?}", s.bands, s.gain_db);
        assert!(s.gain_db[3] > s.gain_db[1] + 20, "{:?}", s.gain_db);
    }

    #[test]
    fn empty_band_stays_dark() {
        // Bass only, plus noise far below the gate: the high band holds
        // instead of its AGC lifting the noise into view (range 48 dB).
        let p = Params { range_db: 48, ..Params::default() };
        let mut seed = 1u32;
        let s = *run(&p, 40, |n| {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            sine(FS / 8, 60, n) + ((seed >> 16) as i32 - 32768) * (FS / 10_000) / 32768
        })
        .last()
        .unwrap();
        assert!(s.bands[0] > 100, "{:?}", s.bands);
        assert!(s.bands[2] < 10, "{:?} gains {:?}", s.bands, s.gain_db);
    }

    #[test]
    fn stuck_input_is_a_fault() {
        let s = run(&Params::default(), 3, |_| 12345);
        assert_eq!(s.last().unwrap().status, Status::Fault);
        assert_eq!(encode(s.last().unwrap())[13], 64);
    }

    #[test]
    fn full_scale_is_clipping() {
        let s = run(&Params::default(), 1, |n| sine(FS, 1000, n));
        assert_eq!(s.last().unwrap().status, Status::Clipping);
    }

    #[test]
    fn control_overrides() {
        let p = Params::default();
        let (q, c) = apply_control(&p, &[0; CONTROL_CHANNELS]);
        assert_eq!((q, c), (p, Control::default()));
        let (q, c) = apply_control(&p, &[1, 255, 1, 255, 1, 255, 64, 0, 255, 200, 128]);
        assert!(!q.agc);
        assert_eq!((q.gain_db, q.gate_db, q.attack_ms, q.release_ms, q.sensitivity), (40, -90, 500, 10, 10));
        assert_eq!(q.band_gain, [64, 128, 255]);
        assert_eq!(c, Control { freeze: true, tap: true });
        assert!(apply_control(&p, &[128, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]).0.agc);
    }

    #[test]
    fn band_gain_scales() {
        let p = Params { agc: false, ..Params::default() };
        let half = Params { band_gain: [64, 128, 128], ..p };
        let a = *run(&p, 1, |n| sine(FS / 8, 60, n)).last().unwrap();
        let b = *run(&half, 1, |n| sine(FS / 8, 60, n)).last().unwrap();
        assert!(b.bands[0].abs_diff(a.bands[0] / 2) <= 1, "{} vs {}", b.bands[0], a.bands[0]);
    }

    #[test]
    fn taps_set_the_tempo() {
        let mut a = Analyzer::new();
        let p = Params::default();
        let mut beats = 0;
        for n in 0..4 * RATE {
            if n % (RATE / 2) == RATE / 4 {
                a.tap();
            }
            if let Some(s) = a.push(0, &p)
                && s.beat
            {
                beats += 1;
            }
        }
        assert!(beats >= 8, "{beats} beat frames");
        assert!(a.period.abs_diff(RATE / 2) < 10, "period {}", a.period);
    }

    #[test]
    fn channel_map() {
        let s = Snapshot {
            level: 0x1234,
            peak: 7,
            beat: true,
            phase: 99,
            bands: [1, 2, 3],
            onsets: [true, false, true],
            activity: Activity::Music,
            input_db: 0,
            beat_ms: 500,
            gain_db: [0; 4],
            status: Status::Active,
        };
        assert_eq!(encode(&s), [0x12, 0x34, 7, 255, 99, 1, 2, 3, 255, 0, 255, 255, 255, 192, 0, 0]);
        assert_eq!(encode(&Snapshot { input_db: -96, ..s })[12], 0);
    }
}
