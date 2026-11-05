//! The user settings and their form format.
//!
//! Settings are kept as a URL-encoded form string (`name=…&eth=dhcp…`), the
//! same format the settings page posts, so one parser serves both. Unknown
//! keys are ignored and missing ones take their defaults, so the format can
//! grow without migrations. Storage and the settings in effect are in the
//! firmware (`esp32_pixelcast::settings`).

use alloc::string::{String, ToString};
use core::net::Ipv4Addr;

use log::warn;

use crate::{
    artnet, config, http,
    led::{self, LedType, Order, OutputCfg, Timing},
};

// The error messages below name this limit.
const _: () = assert!(led::MAX_LEDS == 4096);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EthMode {
    Dhcp,
    Static,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    /// Device name; also the setup Wi-Fi name. Empty means "ArtnetNode-XXXX".
    pub name: String,
    /// Password for the settings page and the setup Wi-Fi. Empty means the
    /// build-time default.
    pub password: String,
    pub eth_mode: EthMode,
    pub ip: Ipv4Addr,
    pub prefix: u8,
    pub gateway: Option<Ipv4Addr>,
    /// Use a fallback address when DHCP gets no answer.
    pub artnet_fallback: bool,
    /// That address; `None`: Art-Net's 2.x.y.z from the MAC, with
    /// `fallback_prefix` (8 for Art-Net).
    pub fallback_ip: Option<Ipv4Addr>,
    pub fallback_prefix: u8,
    pub ap_channel: u8,
    /// Start the setup Wi-Fi after the Ethernet link has been down this long.
    pub ap_after_s: u32,
    /// Start the setup Wi-Fi at every start-up for this many minutes (0: no).
    pub ap_boot_min: u16,
    /// ...and end that window only after this long without a web request
    /// over the setup Wi-Fi.
    pub ap_idle_min: u16,
    /// LED type and colour order on the wire (W, if any, last).
    pub led_type: LedType,
    pub order: Order,
    /// Overall brightness, 1..=100 %.
    pub brightness: u8,
    /// Per output: LED count and first universe.
    pub outputs: [OutputCfg; led::LANES],
    /// Bit timing (the "overclock" setting).
    pub timing: Timing,
    /// Low time after each frame that makes the LEDs show it.
    pub latch_us: u32,
    /// Which protocol the LED data arrives in.
    pub protocol: Protocol,
    /// Blend between input frames and send at the strips' full rate.
    pub interpolate: bool,
    /// Temporal dithering (smoother dark fades).
    pub dither: bool,
    /// Run the flash at QIO 80 MHz after start-up (see `flashinfo`).
    pub fast_flash: bool,
    /// Art-Net merge of two sources on one universe.
    pub merge: crate::merge::Mode,
    /// Saved LED timing presets: `name|period|t0h|t1h|latch` joined by `;`
    /// (checked by [`parse_presets`]).
    pub timing_presets: String,
    /// What the outputs do when no data arrives.
    pub loss: Loss,
    /// Seconds without data before `loss` applies.
    pub loss_after_s: u16,
    /// Colour (R, G, B, W) for [`Loss::Color`].
    pub loss_color: [u8; 4],
    /// Power model: mA per channel at full value; limits in mA (0 = none).
    /// Power of one LED (pixel) at full white, mW.
    pub power_mw_per_led: u32,
    /// Power limit per output and for all outputs, mW (0 = none).
    pub power_out_mw: u32,
    pub power_total_mw: u32,
    /// Strip voltage in 0.1 V, only to show the current per output.
    pub led_decivolts: u16,
    /// The microphone as a DMX source.
    pub audio: Audio,
}

/// The kind of microphone (or line-in ADC) on the I2S pins.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MicType {
    /// 24/32-bit I2S: SPH0645, INMP441, ICS-43434, ICS-43432 …
    I2s,
    /// The same with a master clock on GPIO 3 (line-in ADCs like PCM1808).
    I2sMclk,
    /// PDM: SPM1423, MSM261 … (clock on the WS pin, no BCLK).
    Pdm,
}

impl MicType {
    pub fn key(self) -> &'static str {
        match self {
            MicType::I2s => "i2s",
            MicType::I2sMclk => "i2s_mclk",
            MicType::Pdm => "pdm",
        }
    }
}

/// The mic's wiring. Pins apply after a restart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mic {
    pub kind: MicType,
    /// The mic's SEL (L/R) pin is tied to 3.3 V (right channel) instead of GND.
    pub sel_high: bool,
    /// GPIOs: data in, word select (PDM: clock), bit clock (unused for PDM).
    pub din: u8,
    pub ws: u8,
    pub bclk: u8,
}

impl Default for Mic {
    fn default() -> Self {
        // This board's header: SPH0645 with SEL to GND.
        Mic { kind: MicType::I2s, sel_high: false, din: 32, ws: 15, bclk: 33 }
    }
}

/// GPIOs free for the mic on this board; the first five can also drive
/// a clock (34..39 are inputs only).
pub const MIC_PINS: [u8; 9] = [2, 13, 15, 32, 33, 34, 35, 36, 39];
pub const MIC_OUTPUT_PINS: [u8; 5] = [2, 13, 15, 32, 33];

impl Mic {
    pub fn validate(&self) -> Result<(), &'static str> {
        if !MIC_PINS.contains(&self.din) {
            return Err("The mic data pin must be one of GPIO 2, 13, 15, 32, 33, 34, 35, 36, 39.");
        }
        if !MIC_OUTPUT_PINS.contains(&self.ws) {
            return Err("The mic WS/clock pin must be one of GPIO 2, 13, 15, 32, 33.");
        }
        if self.kind != MicType::Pdm {
            if !MIC_OUTPUT_PINS.contains(&self.bclk) {
                return Err("The mic BCLK pin must be one of GPIO 2, 13, 15, 32, 33.");
            }
            if self.bclk == self.ws || self.bclk == self.din {
                return Err("The mic pins must be different.");
            }
        }
        if self.ws == self.din {
            return Err("The mic pins must be different.");
        }
        Ok(())
    }
}

/// The microphone's analysis, sent as a DMX universe (`crate::audio`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Audio {
    pub on: bool,
    pub mic: Mic,
    /// How the audio universe is sent.
    pub protocol: Protocol,
    /// Unicast to this address; `None`: Art-Net broadcast, sACN multicast.
    pub dest: Option<Ipv4Addr>,
    /// Port-address (sACN universe = this + 1, as on the receive side).
    pub universe: u16,
    /// sACN priority, 1..=200.
    pub priority: u8,
    pub params: crate::audio::Params,
    /// Control channels from the console: in the input protocol, this
    /// universe (port-address) from DMX address `control_address` on.
    pub control: bool,
    pub control_universe: u16,
    pub control_address: u16,
}

impl Default for Audio {
    fn default() -> Self {
        Audio {
            on: false,
            mic: Mic::default(),
            protocol: Protocol::ArtNet,
            dest: None,
            // The default LED setup uses 0..=15.
            universe: 16,
            priority: 100,
            params: crate::audio::Params::default(),
            control: false,
            control_universe: 17,
            control_address: 1,
        }
    }
}

/// Behaviour when the input stops.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Loss {
    /// Keep the last frame.
    Hold,
    /// Fade to black (2 s).
    Fade,
    /// Show `loss_color`.
    Color,
}

impl Loss {
    pub fn key(self) -> &'static str {
        match self {
            Loss::Hold => "hold",
            Loss::Fade => "fade",
            Loss::Color => "color",
        }
    }
}

/// The DMX-over-IP protocol to listen for. One at a time: both share the
/// receive buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Protocol {
    /// Art-Net on UDP 6454.
    ArtNet,
    /// sACN (E1.31) on UDP 5568, multicast and unicast; sACN universe 1
    /// feeds port-address 0.
    Sacn,
}

impl Protocol {
    pub fn key(self) -> &'static str {
        match self {
            Protocol::ArtNet => "artnet",
            Protocol::Sacn => "sacn",
        }
    }
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            name: String::new(),
            password: String::new(),
            eth_mode: EthMode::Dhcp,
            ip: Ipv4Addr::new(2, 0, 0, 10),
            prefix: 8,
            gateway: None,
            artnet_fallback: true,
            fallback_ip: None,
            fallback_prefix: 8,
            ap_channel: config::AP_CHANNEL,
            ap_after_s: (config::AP_AFTER_LINK_DOWN_MS / 1000) as u32,
            ap_boot_min: 0,
            ap_idle_min: 5,
            // This board's typical setup: SK6812 RGBW, 4 × 512 LEDs on
            // universes 0-3, 4-7, 8-11, 12-15.
            led_type: LedType::Rgbw,
            order: Order([1, 0, 2]),
            brightness: 100,
            outputs: core::array::from_fn(|k| OutputCfg::new(512, (k * 4) as u16)),
            timing: Timing::SK6812,
            latch_us: 80,
            protocol: Protocol::ArtNet,
            interpolate: false,
            dither: false,
            fast_flash: false,
            merge: crate::merge::Mode::Htp,
            timing_presets: String::new(),
            loss: Loss::Hold,
            loss_after_s: 5,
            loss_color: [0, 0, 0, 0],
            // 5 V, 20 mA a channel, RGB.
            power_mw_per_led: 300,
            power_out_mw: 0,
            power_total_mw: 0,
            led_decivolts: 50,
            audio: Audio::default(),
        }
    }
}

impl Settings {
    /// The password in effect.
    pub fn password(&self) -> &str {
        if self.password.is_empty() {
            config::PASSWORD
        } else {
            &self.password
        }
    }

    /// The device name in effect; `mac` is the Wi-Fi MAC for the default.
    pub fn name(&self, mac: [u8; 6]) -> String {
        if self.name.is_empty() {
            alloc::format!("ArtnetNode-{:02X}{:02X}", mac[4], mac[5])
        } else {
            self.name.clone()
        }
    }

    /// Applies form fields onto `self`. With `strict`, any invalid value is
    /// an error (settings page); without, invalid values keep the current
    /// value (loading from flash). An empty password field keeps the current
    /// password.
    pub fn apply_form(&mut self, form: &str, strict: bool) -> Result<(), &'static str> {
        let mut next = self.clone();
        let mut password2: Option<String> = None;
        for (key, value) in http::form_pairs(form) {
            let r: Result<(), &'static str> = match key.as_str() {
                "name" => valid_name(&value).map(|()| next.name = value),
                "password" if value.is_empty() => Ok(()),
                "password" => valid_password(&value).map(|()| next.password = value),
                "password2" => {
                    password2 = Some(value);
                    Ok(())
                }
                "eth" => match value.as_str() {
                    "dhcp" => Ok(next.eth_mode = EthMode::Dhcp),
                    "static" => Ok(next.eth_mode = EthMode::Static),
                    _ => Err("Ethernet mode must be DHCP or static."),
                },
                "ip" => match value.parse() {
                    Ok(ip) if usable_ip(ip) => Ok(next.ip = ip),
                    _ => Err("The IP address is not valid (it must be a normal unicast address)."),
                },
                "prefix" => match value.parse::<u8>() {
                    Ok(p @ 1..=30) => Ok(next.prefix = p),
                    _ => Err("The prefix length must be 1 to 30."),
                },
                "gateway" if value.is_empty() => Ok(next.gateway = None),
                "gateway" => match value.parse() {
                    Ok(ip) if usable_ip(ip) => Ok(next.gateway = Some(ip)),
                    _ => Err("The gateway address is not valid."),
                },
                "fallback" => Ok(next.artnet_fallback = value == "1"),
                "fallback_ip" if value.is_empty() => Ok(next.fallback_ip = None),
                "fallback_ip" => match value.parse() {
                    Ok(ip) if usable_ip(ip) => Ok(next.fallback_ip = Some(ip)),
                    _ => Err("The fallback address is not valid (a normal unicast address, or empty for Art-Net's 2.x.y.z)."),
                },
                "fallback_prefix" => match value.parse::<u8>() {
                    Ok(p @ 1..=30) => Ok(next.fallback_prefix = p),
                    _ => Err("The fallback prefix length must be 1 to 30."),
                },
                "ap_channel" => match value.parse::<u8>() {
                    Ok(c @ 1..=13) => Ok(next.ap_channel = c),
                    _ => Err("The Wi-Fi channel must be 1 to 13."),
                },
                "ap_after" => match value.parse::<u32>() {
                    Ok(s @ 5..=3600) => Ok(next.ap_after_s = s),
                    _ => Err("The Wi-Fi start delay must be 5 to 3600 seconds."),
                },
                "ap_boot" => match value.parse::<u16>() {
                    Ok(m @ 0..=1440) => Ok(next.ap_boot_min = m),
                    _ => Err("The Wi-Fi time after start-up must be 0 to 1440 minutes."),
                },
                "ap_idle" => match value.parse::<u16>() {
                    Ok(m @ 1..=240) => Ok(next.ap_idle_min = m),
                    _ => Err("The Wi-Fi idle time must be 1 to 240 minutes."),
                },
                "led_type" => match value.as_str() {
                    "rgbw" => Ok(next.led_type = LedType::Rgbw),
                    "rgb" => Ok(next.led_type = LedType::Rgb),
                    _ => Err("The LED type must be RGB or RGBW."),
                },
                "order" => Order::parse(&value)
                    .map(|o| next.order = o)
                    .ok_or("The colour order must be a mix of r, g and b, like grb."),
                "brightness" => match value.parse::<u8>() {
                    Ok(b @ 1..=100) => Ok(next.brightness = b),
                    _ => Err("The brightness must be 1 to 100 %."),
                },
                "t0h" => parse_ns(&value).map(|v| next.timing.t0h_ns = v),
                "t1h" => parse_ns(&value).map(|v| next.timing.t1h_ns = v),
                "period" => parse_ns(&value).map(|v| next.timing.period_ns = v),
                "latch" => match value.parse::<u32>() {
                    Ok(l @ 50..=1000) => Ok(next.latch_us = l),
                    _ => Err("The latch time must be 50 to 1000 µs."),
                },
                "interp" => Ok(next.interpolate = value == "1"),
                "dither" => Ok(next.dither = value == "1"),
                "fast_flash" => Ok(next.fast_flash = value == "1"),
                "presets" => match parse_presets(&value) {
                    Some(_) => Ok(next.timing_presets = value.clone()),
                    None => Err("Presets: up to 8, each a name (1–24 characters, no | ; \" \\) and valid timing."),
                },
                "merge" => crate::merge::Mode::parse(&value)
                    .map(|m| next.merge = m)
                    .ok_or("The merge mode must be HTP or LTP."),
                "loss" => match value.as_str() {
                    "hold" => Ok(next.loss = Loss::Hold),
                    "fade" => Ok(next.loss = Loss::Fade),
                    "color" => Ok(next.loss = Loss::Color),
                    _ => Err("Signal loss must be hold, fade or color."),
                },
                "loss_after" => match value.parse::<u16>() {
                    Ok(s @ 1..=3600) => Ok(next.loss_after_s = s),
                    _ => Err("The signal-loss delay must be 1 to 3600 seconds."),
                },
                "loss_color" => parse_rgbw(&value).map(|c| next.loss_color = c),
                "power_led_mw" => match value.parse::<u32>() {
                    Ok(m @ 1..=50_000) => Ok(next.power_mw_per_led = m),
                    _ => Err("The power per LED must be 0.001 to 50 W."),
                },
                "power_out_mw" => match value.parse::<u32>() {
                    Ok(m @ 0..=10_000_000) => Ok(next.power_out_mw = m),
                    _ => Err("The per-output limit must be 0 to 10000 W."),
                },
                "led_volts_dv" => match value.parse::<u16>() {
                    Ok(v @ 10..=600) => Ok(next.led_decivolts = v),
                    _ => Err("The strip voltage must be 1 to 60 V."),
                },
                "power_total_mw" => match value.parse::<u32>() {
                    Ok(m @ 0..=40_000_000) => Ok(next.power_total_mw = m),
                    _ => Err("The total limit must be 0 to 40000 W."),
                },
                // Settings saved before the limit was in watts: mA (per
                // channel, per output, total) at 5 V, three channels.
                "power_ma" => match value.parse::<u32>() {
                    Ok(m @ 1..=100) => Ok(next.power_mw_per_led = m * 3 * 5),
                    _ => Err("mA per channel must be 1 to 100."),
                },
                "power_out" => match value.parse::<u32>() {
                    Ok(m @ 0..=100_000) => Ok(next.power_out_mw = m * 5),
                    _ => Err("The per-output limit must be 0 to 100000 mA."),
                },
                "power_total" => match value.parse::<u32>() {
                    Ok(m @ 0..=400_000) => Ok(next.power_total_mw = m * 5),
                    _ => Err("The total limit must be 0 to 400000 mA."),
                },
                "protocol" => match value.as_str() {
                    "artnet" => Ok(next.protocol = Protocol::ArtNet),
                    "sacn" => Ok(next.protocol = Protocol::Sacn),
                    _ => Err("The protocol must be Art-Net or sACN."),
                },
                k if k.starts_with("au_") => apply_audio(&mut next.audio, k, &value),
                k if output_key(k).is_some() => {
                    let (lane, field) = output_key(k).unwrap_or((0, ""));
                    let o = &mut next.outputs[lane];
                    match field {
                        "leds" => match value.parse::<u16>() {
                            Ok(n) if usize::from(n) <= led::MAX_LEDS => Ok(o.leds = n),
                            _ => Err("LEDs per output must be 0 to 4096."),
                        },
                        "univ" => match value.parse::<u16>() {
                            Ok(u) if u <= 0x7fff => Ok(o.universe = u),
                            _ => Err("Universes must be 0 to 32767."),
                        },
                        "type" => match value.as_str() {
                            "" => Ok(o.ty = None),
                            "rgbw" => Ok(o.ty = Some(LedType::Rgbw)),
                            "rgb" => Ok(o.ty = Some(LedType::Rgb)),
                            _ => Err("An output's LED type must be RGB, RGBW or empty (same as all)."),
                        },
                        "order" if value.is_empty() => Ok(o.order = None),
                        "order" => Order::parse(&value)
                            .map(|v| o.order = Some(v))
                            .ok_or("An output's colour order must be a mix of r, g and b, or empty."),
                        "rev" => Ok(o.reverse = value == "1"),
                        "group" => match value.parse::<u8>() {
                            Ok(g @ 1..=64) => Ok(o.group = g),
                            _ => Err("Grouping must be 1 to 64 LEDs per pixel."),
                        },
                        "offset" => match value.parse::<u16>() {
                            Ok(n) if usize::from(n) <= led::MAX_LEDS => Ok(o.offset = n),
                            _ => Err("The offset must be 0 to 4096 LEDs."),
                        },
                        "start" => match value.parse::<u16>() {
                            Ok(n) if (1..=artnet::DMX_CHANNELS as u16).contains(&n) => Ok(o.start = n),
                            _ => Err("The DMX start address must be 1 to 512."),
                        },
                        _ => Ok(()),
                    }
                }
                _ => Ok(()),
            };
            if let Err(e) = r {
                if strict {
                    return Err(e);
                }
                warn!("settings: ignoring {}: {}", key, e);
            }
        }
        if strict {
            // The page sends `fallback` only when checked, so on a full form
            // (recognised by `eth`) its absence means "off". A partial post
            // leaves it alone.
            if form_has(form, "eth") && !form_has(form, "fallback") {
                next.artnet_fallback = false;
            }
            if next.password != self.password && password2.as_deref() != Some(next.password.as_str()) {
                return Err("The two passwords do not match.");
            }
            if next.eth_mode == EthMode::Static && next.ip.is_unspecified() {
                return Err("Set an IP address for static mode.");
            }
            if form_has(form, "au_univ") && !form_has(form, "au_on") {
                next.audio.on = false;
            }
            if form_has(form, "au_univ") && !form_has(form, "au_agc") {
                next.audio.params.agc = false;
            }
            if form_has(form, "au_univ") && !form_has(form, "au_ctl") {
                next.audio.control = false;
            }
            next.audio_collision()?;
            if next.audio.control && next.audio.on && next.audio.control_universe == next.audio.universe {
                return Err("The audio control universe must differ from the one the audio is sent on.");
            }
            next.audio.mic.validate()?;
            next.timing.validate()?;
        } else if let Err(e) = next.timing.validate() {
            warn!("settings: stored LED timing invalid ({}); using the default", e);
            next.timing = Timing::SK6812;
        }
        *self = next;
        Ok(())
    }

    /// The form string that is stored in flash. Lists every field (a new
    /// one does not compile until it is written here; the round-trip test
    /// checks that `apply_form` reads it back).
    pub fn to_form(&self) -> String {
        let Settings {
            name,
            password,
            eth_mode,
            ip,
            prefix,
            gateway,
            artnet_fallback,
            fallback_ip,
            fallback_prefix,
            ap_channel,
            ap_after_s,
            ap_boot_min,
            ap_idle_min,
            led_type,
            order,
            brightness,
            outputs,
            timing,
            latch_us,
            protocol,
            interpolate,
            dither,
            fast_flash,
            merge,
            timing_presets,
            loss,
            loss_after_s,
            loss_color,
            power_mw_per_led,
            power_out_mw,
            power_total_mw,
            led_decivolts,
            audio,
        } = self;
        let mut s = String::new();
        let mut kv = |k: &str, v: &str| {
            if !s.is_empty() {
                s.push('&');
            }
            s.push_str(k);
            s.push('=');
            http::url_encode_into(v, &mut s);
        };
        let flag = |b: bool| if b { "1" } else { "0" };
        kv("name", name);
        kv("password", password);
        kv("eth", if *eth_mode == EthMode::Static { "static" } else { "dhcp" });
        kv("ip", &ip.to_string());
        kv("prefix", &prefix.to_string());
        kv("gateway", &gateway.map(|g| g.to_string()).unwrap_or_default());
        kv("fallback", flag(*artnet_fallback));
        kv("fallback_ip", &fallback_ip.map(|a| a.to_string()).unwrap_or_default());
        kv("fallback_prefix", &fallback_prefix.to_string());
        kv("ap_channel", &ap_channel.to_string());
        kv("ap_after", &ap_after_s.to_string());
        kv("ap_boot", &ap_boot_min.to_string());
        kv("ap_idle", &ap_idle_min.to_string());
        kv("led_type", led_type.key());
        kv("order", core::str::from_utf8(&order.name()).unwrap_or("grb"));
        kv("brightness", &brightness.to_string());
        for (k, o) in outputs.iter().enumerate() {
            let OutputCfg { leds, universe, ty, order, reverse, group, offset, start } = o;
            let key = |f: &str| alloc::format!("o{}_{}", k + 1, f);
            kv(&key("leds"), &leds.to_string());
            kv(&key("univ"), &universe.to_string());
            kv(&key("type"), ty.map_or("", LedType::key));
            let order = order.map(|v| v.name());
            kv(&key("order"), order.as_ref().and_then(|n| core::str::from_utf8(n).ok()).unwrap_or(""));
            kv(&key("rev"), flag(*reverse));
            kv(&key("group"), &group.to_string());
            kv(&key("offset"), &offset.to_string());
            kv(&key("start"), &start.to_string());
        }
        let Timing { t0h_ns, t1h_ns, period_ns } = timing;
        kv("t0h", &t0h_ns.to_string());
        kv("t1h", &t1h_ns.to_string());
        kv("period", &period_ns.to_string());
        kv("latch", &latch_us.to_string());
        kv("protocol", protocol.key());
        kv("interp", flag(*interpolate));
        kv("dither", flag(*dither));
        kv("fast_flash", flag(*fast_flash));
        kv("merge", merge.key());
        kv("presets", timing_presets);
        kv("loss", loss.key());
        kv("loss_after", &loss_after_s.to_string());
        kv("loss_color", &rgbw_hex(*loss_color));
        kv("power_led_mw", &power_mw_per_led.to_string());
        kv("power_out_mw", &power_out_mw.to_string());
        kv("power_total_mw", &power_total_mw.to_string());
        kv("led_volts_dv", &led_decivolts.to_string());
        let Audio { on, mic, protocol, dest, universe, priority, params, control, control_universe, control_address } = audio;
        let Mic { kind, sel_high, din, ws, bclk } = mic;
        // band_gain: only the control channels set it.
        let crate::audio::Params {
            agc,
            gain_db,
            gate_db,
            attack_ms,
            release_ms,
            sensitivity,
            band_gain: _,
            low_hz,
            high_hz,
            range_db,
        } = params;
        kv("au_on", flag(*on));
        kv("au_mic", kind.key());
        kv("au_sel", if *sel_high { "vdd" } else { "gnd" });
        kv("au_din", &din.to_string());
        kv("au_ws", &ws.to_string());
        kv("au_bclk", &bclk.to_string());
        kv("au_proto", protocol.key());
        kv("au_dest", &dest.map(|d| d.to_string()).unwrap_or_default());
        kv("au_univ", &universe.to_string());
        kv("au_prio", &priority.to_string());
        kv("au_agc", flag(*agc));
        kv("au_gain", &gain_db.to_string());
        kv("au_gate", &gate_db.to_string());
        kv("au_attack", &attack_ms.to_string());
        kv("au_release", &release_ms.to_string());
        kv("au_sens", &sensitivity.to_string());
        kv("au_low", &low_hz.to_string());
        kv("au_high", &high_hz.to_string());
        kv("au_range", &range_db.to_string());
        kv("au_ctl", flag(*control));
        kv("au_ctl_univ", &control_universe.to_string());
        kv("au_ctl_addr", &control_address.to_string());
        s
    }

    /// An LED output receives the audio universe or the control universe
    /// (also checked when an ArtAddress moves the outputs).
    pub fn audio_collision(&self) -> Result<(), &'static str> {
        let a = &self.audio;
        if a.control && self.led_universes().any(|u| u == a.control_universe) {
            return Err("The audio control universe is one the LED outputs receive: choose another.");
        }
        if a.on && self.led_universes().any(|u| u == a.universe) {
            return Err("The audio universe is one the LED outputs receive: choose another.");
        }
        Ok(())
    }

    /// The port-addresses the LED outputs receive.
    pub fn led_universes(&self) -> impl Iterator<Item = u16> + '_ {
        self.outputs
            .iter()
            .flat_map(|o| (0..o.universes(self.led_type) as u16).map(move |k| o.universe.wrapping_add(k) & 0x7fff))
    }
}

/// One `au_*` form field.
fn apply_audio(a: &mut Audio, key: &str, value: &str) -> Result<(), &'static str> {
    let p = &mut a.params;
    let pin = |v: &str| v.parse::<u8>().ok().filter(|n| MIC_PINS.contains(n)).ok_or("Mic pins must be one of GPIO 2, 13, 15, 32, 33, 34, 35, 36, 39.");
    match key {
        "au_on" => a.on = value == "1",
        "au_mic" => {
            a.mic.kind = match value {
                "i2s" => MicType::I2s,
                "i2s_mclk" => MicType::I2sMclk,
                "pdm" => MicType::Pdm,
                _ => return Err("The mic type must be I2S, I2S with MCLK or PDM."),
            }
        }
        "au_sel" => a.mic.sel_high = value == "vdd",
        "au_din" => a.mic.din = pin(value)?,
        "au_ws" => a.mic.ws = pin(value)?,
        "au_bclk" => a.mic.bclk = pin(value)?,
        "au_proto" => {
            a.protocol = match value {
                "artnet" => Protocol::ArtNet,
                "sacn" => Protocol::Sacn,
                _ => return Err("The audio protocol must be Art-Net or sACN."),
            }
        }
        "au_dest" if value.is_empty() => a.dest = None,
        "au_dest" => match value.parse() {
            Ok(ip) if usable_ip(ip) => a.dest = Some(ip),
            _ => return Err("The audio destination must be a unicast address, or empty."),
        },
        "au_univ" => match value.parse::<u16>() {
            Ok(u) if u <= 0x7fff => a.universe = u,
            _ => return Err("The audio universe must be 0 to 32767."),
        },
        "au_prio" => match value.parse::<u8>() {
            Ok(v @ 1..=200) => a.priority = v,
            _ => return Err("The sACN priority must be 1 to 200."),
        },
        "au_agc" => p.agc = value == "1",
        "au_gain" => match value.parse::<i8>() {
            Ok(v @ -20..=40) => p.gain_db = v,
            _ => return Err("The audio gain must be -20 to 40 dB."),
        },
        "au_gate" => match value.parse::<i8>() {
            Ok(v @ -90..=-20) => p.gate_db = v,
            _ => return Err("The noise gate must be -90 to -20 dBFS."),
        },
        "au_attack" => match value.parse::<u16>() {
            Ok(v @ 1..=500) => p.attack_ms = v,
            _ => return Err("The attack must be 1 to 500 ms."),
        },
        "au_release" => match value.parse::<u16>() {
            Ok(v @ 10..=3000) => p.release_ms = v,
            _ => return Err("The release must be 10 to 3000 ms."),
        },
        "au_ctl" => a.control = value == "1",
        "au_ctl_univ" => match value.parse::<u16>() {
            Ok(u) if u <= 0x7fff => a.control_universe = u,
            _ => return Err("The audio control universe must be 0 to 32767."),
        },
        "au_ctl_addr" => match value.parse::<u16>() {
            Ok(v) if (1..=(512 - crate::audio::CONTROL_CHANNELS as u16 + 1)).contains(&v) => a.control_address = v,
            _ => return Err("The audio control address must be 1 to 502."),
        },
        "au_low" => match value.parse::<u16>() {
            Ok(v @ 40..=500) => p.low_hz = v,
            _ => return Err("The low/mid crossover must be 40 to 500 Hz."),
        },
        "au_high" => match value.parse::<u16>() {
            Ok(v @ 1000..=8000) => p.high_hz = v,
            _ => return Err("The mid/high crossover must be 1000 to 8000 Hz."),
        },
        "au_range" => match value.parse::<u8>() {
            Ok(v @ 12..=48) => p.range_db = v,
            _ => return Err("The range must be 12 to 48 dB."),
        },
        "au_sens" => match value.parse::<u8>() {
            Ok(v @ 1..=10) => p.sensitivity = v,
            _ => return Err("The beat sensitivity must be 1 to 10."),
        },
        _ => {}
    }
    Ok(())
}

/// `o1_leds` … `o4_offset` → (output index, field).
fn output_key(k: &str) -> Option<(usize, &str)> {
    const FIELDS: [&str; 8] = ["leds", "univ", "type", "order", "rev", "group", "offset", "start"];
    let rest = k.strip_prefix('o')?;
    let (num, field) = rest.split_once('_')?;
    let n: usize = num.parse().ok()?;
    ((1..=led::LANES).contains(&n) && FIELDS.contains(&field)).then_some((n - 1, field))
}

/// `rrggbbww` (hex) → [R, G, B, W].
fn parse_rgbw(v: &str) -> Result<[u8; 4], &'static str> {
    const ERR: &str = "The colour must be 8 hex digits (rrggbbww).";
    let v = v.trim_start_matches('#');
    // ASCII first: byte slicing below must not split a character.
    if !v.is_ascii() || v.len() != 8 {
        return Err(ERR);
    }
    let mut c = [0u8; 4];
    for (i, b) in c.iter_mut().enumerate() {
        *b = u8::from_str_radix(&v[i * 2..i * 2 + 2], 16).map_err(|_| ERR)?;
    }
    Ok(c)
}

pub fn rgbw_hex(c: [u8; 4]) -> String {
    alloc::format!("{:02x}{:02x}{:02x}{:02x}", c[0], c[1], c[2], c[3])
}

/// One saved LED timing preset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Preset<'a> {
    pub name: &'a str,
    pub period: u32,
    pub t0h: u32,
    pub t1h: u32,
    pub latch: u32,
}

/// The saved timing presets (at most 8).
pub type Presets<'a> = heapless::Vec<Preset<'a>, 8>;

/// Up to 8 presets `name|period|t0h|t1h|latch;…` (empty = none), with the
/// same ranges as the timing settings. `None` if anything is off.
pub fn parse_presets(v: &str) -> Option<Presets<'_>> {
    let mut list = Presets::new();
    if v.is_empty() {
        return Some(list);
    }
    for item in v.split(';') {
        let mut f = item.split('|');
        let name = f.next()?;
        let mut num = || f.next()?.parse::<u32>().ok();
        let p = Preset { name, period: num()?, t0h: num()?, t1h: num()?, latch: num()? };
        let name_ok = (1..=24).contains(&name.len())
            && name.bytes().all(|b| (0x20..0x7f).contains(&b) && !matches!(b, b'|' | b';' | b'"' | b'\\'));
        let ok = name_ok
            && (500..=3000).contains(&p.period)
            && (100..=1000).contains(&p.t0h)
            && (150..=2000).contains(&p.t1h)
            && p.t0h < p.t1h
            && (50..=1000).contains(&p.latch);
        if !ok || f.next().is_some() || list.push(p).is_err() {
            return None;
        }
    }
    Some(list)
}


fn parse_ns(v: &str) -> Result<u32, &'static str> {
    v.parse::<u32>().map_err(|_| "Timing values are whole nanoseconds.")
}

fn form_has(form: &str, key: &str) -> bool {
    http::form_pairs(form).any(|(k, _)| k == key)
}

fn valid_name(v: &str) -> Result<(), &'static str> {
    // Also the Wi-Fi name: at most 32 bytes, printable ASCII keeps it simple.
    if v.len() > 32 || !v.bytes().all(|b| (0x20..0x7f).contains(&b)) {
        return Err("The name must be up to 32 plain ASCII characters.");
    }
    Ok(())
}

fn valid_password(v: &str) -> Result<(), &'static str> {
    if !(8..=63).contains(&v.len()) || !v.bytes().all(|b| (0x20..0x7f).contains(&b)) {
        return Err("The password must be 8 to 63 plain characters.");
    }
    Ok(())
}

impl Settings {
    /// Back to the default password and network settings (DHCP, setup
    /// Wi-Fi defaults); the name and the LED settings stay.
    pub fn reset_access(&mut self) {
        let d = Settings::default();
        self.password = d.password;
        self.eth_mode = d.eth_mode;
        self.ip = d.ip;
        self.prefix = d.prefix;
        self.gateway = d.gateway;
        self.artnet_fallback = d.artnet_fallback;
        self.fallback_ip = d.fallback_ip;
        self.fallback_prefix = d.fallback_prefix;
        self.ap_channel = d.ap_channel;
        self.ap_after_s = d.ap_after_s;
        self.ap_boot_min = d.ap_boot_min;
        self.ap_idle_min = d.ap_idle_min;
    }
}

/// Which parts of the firmware a settings change concerns.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Changes {
    /// The Ethernet address settings.
    pub eth: bool,
    /// Name, password or channel of the setup Wi-Fi.
    pub ap: bool,
    /// Anything the LED output is configured from.
    pub led: bool,
    /// Fast flash was switched on (try it again at the next start).
    pub fast_flash_on: bool,
}

impl Changes {
    /// Lists every field (a new one does not compile until it is sorted in
    /// here). Fields marked "read live" are read where they are used and
    /// need no signal.
    pub fn between(old: &Settings, new: &Settings) -> Changes {
        let Settings {
            name,
            password,
            eth_mode,
            ip,
            prefix,
            gateway,
            artnet_fallback,
            fallback_ip,
            fallback_prefix,
            ap_channel,
            ap_after_s: _,  // read live
            ap_boot_min: _, // read at start-up
            ap_idle_min: _, // read live
            led_type,
            order,
            brightness,
            outputs,
            timing,
            latch_us,
            protocol: _, // read live (the receive task rebinds)
            interpolate,
            dither,
            fast_flash,
            merge: _, // read live
            timing_presets: _, // only the settings page uses them
            loss,
            loss_after_s,
            loss_color: _, // read live (each frame)
            power_mw_per_led,
            power_out_mw,
            power_total_mw,
            led_decivolts: _, // display only
            audio: _,         // read live (the audio task, each frame)
        } = new;
        Changes {
            eth: old.eth_mode != *eth_mode
                || old.ip != *ip
                || old.prefix != *prefix
                || old.gateway != *gateway
                || old.artnet_fallback != *artnet_fallback
                || old.fallback_ip != *fallback_ip
                || old.fallback_prefix != *fallback_prefix,
            ap: old.name != *name || old.password != *password || old.ap_channel != *ap_channel,
            led: old.led_type != *led_type
                || old.order != *order
                || old.brightness != *brightness
                || old.outputs != *outputs
                || old.timing != *timing
                || old.latch_us != *latch_us
                || old.interpolate != *interpolate
                || old.dither != *dither
                || old.loss != *loss
                || old.loss_after_s != *loss_after_s
                || old.power_mw_per_led != *power_mw_per_led
                || old.power_out_mw != *power_out_mw
                || old.power_total_mw != *power_total_mw,
            fast_flash_on: *fast_flash && !old.fast_flash,
        }
    }
}

/// An address the network stack accepts as its own: plain unicast.
pub fn usable_ip(ip: Ipv4Addr) -> bool {
    !(ip.is_unspecified() || ip.is_broadcast() || ip.is_multicast() || ip.is_loopback() || ip.octets()[0] == 0)
}


#[cfg(test)]
mod tests {
    use super::*;

    /// Every field away from its default (and valid).
    fn changed() -> Settings {
        let mut s = Settings::default();
        s.name = "Stage left & right".into();
        s.password = "secret pass=1".into();
        s.eth_mode = EthMode::Static;
        s.ip = Ipv4Addr::new(10, 1, 2, 3);
        s.prefix = 24;
        s.gateway = Some(Ipv4Addr::new(10, 1, 2, 254));
        s.artnet_fallback = false;
        s.fallback_ip = Some(Ipv4Addr::new(10, 9, 8, 7));
        s.fallback_prefix = 16;
        s.ap_channel = 11;
        s.ap_after_s = 60;
        s.ap_boot_min = 15;
        s.ap_idle_min = 3;
        s.led_type = LedType::Rgb;
        s.order = Order([2, 1, 0]);
        s.brightness = 42;
        for (k, o) in s.outputs.iter_mut().enumerate() {
            let k16 = k as u16;
            *o = OutputCfg {
                leds: 100 + k16,
                universe: 200 + k16,
                ty: Some(LedType::Rgbw),
                order: Some(Order([0, 2, 1])),
                reverse: true,
                group: 3 + k as u8,
                offset: 7 + k16,
                start: 4 + k16,
            };
        }
        s.timing = Timing { t0h_ns: 250, t1h_ns: 650, period_ns: 1300 };
        s.latch_us = 300;
        s.protocol = Protocol::Sacn;
        s.interpolate = true;
        s.dither = true;
        s.fast_flash = true;
        s.merge = crate::merge::Mode::Ltp;
        s.timing_presets = "Fast|1000|250|500|100;Slow|1250|300|600|300".into();
        s.loss = Loss::Color;
        s.loss_after_s = 30;
        s.loss_color = [1, 2, 3, 4];
        s.power_mw_per_led = 720;
        s.power_out_mw = 60_000;
        s.power_total_mw = 480_000;
        s.led_decivolts = 120;
        s.audio = Audio {
            on: true,
            mic: Mic { kind: MicType::Pdm, sel_high: true, din: 34, ws: 13, bclk: 2 },
            protocol: Protocol::Sacn,
            dest: Some(Ipv4Addr::new(10, 1, 2, 9)),
            universe: 300,
            priority: 150,
            control: true,
            control_universe: 301,
            control_address: 40,
            params: crate::audio::Params {
                agc: false,
                gain_db: 12,
                gate_db: -50,
                attack_ms: 20,
                release_ms: 800,
                sensitivity: 8,
                band_gain: [128; 3],
                low_hz: 220,
                high_hz: 3500,
                range_db: 20,
            },
        };
        s
    }

    #[test]
    fn every_field_changed() {
        // Guards the test itself: a new field left at its default here would
        // round-trip even if `apply_form` ignored it.
        let (d, c) = (Settings::default(), changed());
        assert_ne!(d.outputs[0], c.outputs[0]);
        let (df, cf) = (d.to_form(), c.to_form());
        let pairs = |f: &str| http::form_pairs(f).collect::<alloc::vec::Vec<_>>();
        for ((k, a), (k2, b)) in pairs(&df).into_iter().zip(pairs(&cf)) {
            assert_eq!(k, k2);
            assert_ne!(a, b, "field {k} is the same as the default");
        }
    }

    #[test]
    fn round_trip() {
        let c = changed();
        let form = c.to_form();
        let mut back = Settings::default();
        back.apply_form(&form, false).unwrap();
        assert_eq!(back, c);
        // Strict (the settings page) needs the password twice.
        let mut strict = Settings::default();
        let with_pw2 = alloc::format!("{form}&password2=secret%20pass%3D1");
        strict.apply_form(&with_pw2, true).unwrap();
        assert_eq!(strict, c);
    }

    #[test]
    fn audio_universe_must_not_be_an_led_universe() {
        // The default LED setup receives 0..=15.
        let mut s = Settings::default();
        assert!(s.apply_form("au_on=1&au_univ=3", true).is_err());
        assert!(!s.audio.on);
        s.apply_form("au_on=1&au_univ=16&au_dest=2.0.0.1", true).unwrap();
        assert!(s.audio.on && s.audio.universe == 16);
        // Off is allowed on any universe; a full audio form without au_on is off.
        s.apply_form("au_univ=3&au_agc=1", true).unwrap();
        assert!(!s.audio.on);
    }

    #[test]
    fn control_universe_is_checked() {
        let mut s = Settings::default();
        let base = "au_on=1&au_univ=16&au_dest=2.0.0.1";
        // An LED universe, the audio universe itself, an address past 502.
        assert!(s.apply_form(&alloc::format!("{base}&au_ctl=1&au_ctl_univ=5"), true).is_err());
        assert!(s.apply_form(&alloc::format!("{base}&au_ctl=1&au_ctl_univ=16"), true).is_err());
        assert!(s.apply_form(&alloc::format!("{base}&au_ctl=1&au_ctl_univ=17&au_ctl_addr=503"), true).is_err());
        s.apply_form(&alloc::format!("{base}&au_ctl=1&au_ctl_univ=17&au_ctl_addr=502"), true).unwrap();
        assert!(s.audio.control && s.audio.control_universe == 17 && s.audio.control_address == 502);
        // Moving an output onto it is caught by the same check.
        s.outputs[0].universe = 17;
        assert!(s.audio_collision().is_err());
    }

    #[test]
    fn fallback_is_checked() {
        let mut s = Settings::default();
        assert!(s.apply_form("fallback_prefix=0", true).is_err());
        assert!(s.apply_form("fallback_prefix=31", true).is_err());
        assert!(s.apply_form("fallback_ip=224.0.0.1", true).is_err());
        assert!(s.apply_form("fallback_ip=10.1.2", true).is_err());
        s.apply_form("fallback_ip=10.1.2.3&fallback_prefix=24", true).unwrap();
        assert_eq!((s.fallback_ip, s.fallback_prefix), (Some(core::net::Ipv4Addr::new(10, 1, 2, 3)), 24));
        s.apply_form("fallback_ip=", true).unwrap();
        assert_eq!(s.fallback_ip, None);
    }

    #[test]
    fn mic_pins_are_checked() {
        let mut s = Settings::default();
        assert!(s.apply_form("au_din=12", true).is_err()); // an LED output
        assert!(s.apply_form("au_ws=34", true).is_err()); // input only
        assert!(s.apply_form("au_din=13&au_ws=13", true).is_err());
        s.apply_form("au_mic=pdm&au_din=34&au_ws=13&au_bclk=13", true).unwrap(); // PDM has no BCLK
        assert_eq!((s.audio.mic.kind, s.audio.mic.din, s.audio.mic.ws), (MicType::Pdm, 34, 13));
        assert!(s.apply_form("au_mic=i2s", true).is_err()); // BCLK 13 = WS 13
    }

    #[test]
    fn partial_form_keeps_the_rest() {
        let mut s = changed();
        s.apply_form("brightness=7", true).unwrap();
        let mut want = changed();
        want.brightness = 7;
        assert_eq!(s, want);
    }

    #[test]
    fn full_form_without_fallback_turns_it_off() {
        let mut s = Settings::default();
        assert!(s.artnet_fallback);
        s.apply_form("eth=dhcp", true).unwrap();
        assert!(!s.artnet_fallback);
    }

    #[test]
    fn strict_rejects_and_keeps_the_old_values() {
        let mut s = Settings::default();
        assert!(s.apply_form("brightness=0", true).is_err());
        assert!(s.apply_form("o5_leds=1", true).is_ok(), "unknown keys are ignored");
        assert!(s.apply_form("password=longenough", true).is_err(), "needs password2");
        assert!(s.apply_form("brightness=50&o1_start=0", true).is_err());
        assert_eq!(s, Settings::default());
    }

    #[test]
    fn power_from_old_milliamp_settings() {
        // Saved before the limit was in watts: mA at 5 V, three channels.
        let mut s = Settings::default();
        s.apply_form("power_ma=20&power_out=4000&power_total=10000", false).unwrap();
        assert_eq!((s.power_mw_per_led, s.power_out_mw, s.power_total_mw), (300, 20_000, 50_000));
    }

    #[test]
    fn changes() {
        let d = Settings::default();
        assert_eq!(Changes::between(&d, &d), Changes::default());
        let mut s = d.clone();
        s.ip = Ipv4Addr::new(10, 0, 0, 1);
        assert_eq!(Changes::between(&d, &s), Changes { eth: true, ..Default::default() });
        let mut s = d.clone();
        s.ap_channel = 1;
        assert_eq!(Changes::between(&d, &s), Changes { ap: true, ..Default::default() });
        let mut s = d.clone();
        s.outputs[3].start = 9;
        assert_eq!(Changes::between(&d, &s), Changes { led: true, ..Default::default() });
        let mut s = d.clone();
        s.fast_flash = true;
        assert_eq!(Changes::between(&d, &s), Changes { fast_flash_on: true, ..Default::default() });
        assert_eq!(Changes::between(&s, &d), Changes::default(), "switching it off needs nothing");
        // `changed()` differs in every field: everything fires.
        let c = Changes::between(&d, &changed());
        assert!(c.eth && c.ap && c.led && c.fast_flash_on);
    }

    #[test]
    fn colour_not_ascii() {
        let mut s = Settings::default();
        // 8 bytes, but "€" is 3 of them: must be an error, not a panic.
        assert!(s.apply_form("loss_color=%E2%82%AC%E2%82%ACab", true).is_err());
    }

    #[test]
    fn lenient_skips_bad_values() {
        let mut s = Settings::default();
        s.apply_form("brightness=0&dither=1", false).unwrap();
        assert_eq!(s.brightness, 100);
        assert!(s.dither);
    }
}
