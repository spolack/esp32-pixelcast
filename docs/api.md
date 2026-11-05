# Web pages and HTTP API

The firmware serves three pages and a JSON API on port 80. The pages are in
`firmware/web/`; `firmware/build.rs` minifies and gzips them into the firmware.

## Pages
| Path      | File          | Access |
|-----------|---------------|--------|
| `/`       | `index.html`  | public: device status |
| `/artnet` | `artnet.html` | public: live Art-Net view |
| `/config` | `config.html` | HTTP Basic login (browser prompt, user `admin`): settings, firmware update, maintenance |

Write readable source: at build time (build.rs) the markers are inlined,
then HTML/CSS/JS are minified and gzipped; size limits apply to the result.

Before sending a page, the build replaces the literal markers
`<!--COMMON_CSS-->` with the contents of `common.css` and `<!--COMMON_JS-->`
with the contents of `common.js` (raw text: put the markers inside
`<style>…</style>` and `<script>…</script>`). After that, every page must be a
single self-contained file: no other requests for CSS, JS, fonts, images,
icons or favicons (use inline SVG and `<link rel="icon" href="data:,">`).

## Server limits (important)
- The board serves two connections at a time per network (`web::SOCKETS`).
  A third parallel request can fail with a connection reset. Scripts send
  requests one after another (a small request queue in common.js), retry a
  failed GET once or twice after ~300 ms, and poll at most twice per second
  (exception: the audio meter polls `/api/audio` five times a second while
  it is shown).
- Responses are small; keep each page (after inlining) well under 30 KB.
- No internet access: the board is often reached on its own Wi-Fi
  (192.168.4.1), also from Android's captive-portal "sign in" window (a
  stripped-down WebView). No CDNs, no web fonts, no external anything.
- Plain modern JS (ES2017, fetch, async/await), no frameworks, no build step.

## JSON API
All numbers are integers. Strings may contain any characters: escape them
when putting them into HTML (use textContent).

### `GET /api/status` (public)
```json
{
  "name": "ArtnetNode-423C",
  "firmware": {"name": "esp32-pixelcast", "version": "0.1.0", "slot": "ota_0", "state": "confirmed"},
  "uptime_s": 1234,
  "heap_free": 53088,
  "default_password": true,
  "ethernet": {"present": true, "link": true, "address": "192.168.1.50/24", "source": "DHCP"},
  "wifi": {"on": true, "ssid": "ArtnetNode-423C", "reason": "no Ethernet link", "address": "192.168.4.1"},
  "artnet": {"network": "eth", "window_ms": 1000, "pps": 640, "dmx": 640, "universes": 16,
             "fps_min": 40, "fps_max": 40, "seq_gaps": 0, "sync": 40, "poll": 0, "invalid": 0}
}
```
- `firmware.state`: `"confirmed"` or `"trial"` (new firmware still proving itself).
- `firmware.next_slot`: the slot `otadata` selects for the next start (`null`:
  none, the bootloader then boots `ota_0`).
- `firmware.slots`: both app slots, as in `/api/config` below (`next`: this
  slot boots at the next start).
- `ethernet.present` false = no Ethernet module; `address` may be null;
  `source` is `"DHCP"`, `"static"` or `"fallback"` (or "").
- `wifi.on` false = setup Wi-Fi off (then ssid/reason/address are "").
- `artnet.network`: `"eth"`, `"wifi"` or `""` (none yet). Rates are per second
  over the last `window_ms`.
- `led` (LED output, added with the LED milestone):
  ```json
  "led": {"fps": 48, "underruns": 0, "frame_us": 20600, "samples_per_bit": 4,
          "sample_rate": 3333333, "measured_rate": 3290000, "t0h": 300, "t1h": 600, "period": 1200,
          "power_mw": [14400, 0, 0, 0], "volts_dv": 120, "power_limited": false}
  ```
  `fps` = frames sent to the LEDs per second; `underruns` = frames cut short
  since start (the CPU could not keep up; a few during a settings save or
  firmware update are expected); `frame_us` = longest frame transmission in
  the last second; `sample_rate` = I2S rate in use, `measured_rate` = what
  the start-up self-test measured (a few % low is normal: measurement
  overhead); `t0h`/`t1h`/`period` = the timing actually produced, in ns;
  `power_mw` = estimated power per output (from the pixel data and the power
  per LED), `volts_dv` = the strip voltage from the settings in 0.1 V (current
  per output = `power_mw` / volts), `power_limited` = the power limit dims the
  output.
- `spi` (W5500 bus, diagnostics): `{"mhz": 80, "half_duplex": true, "txn": 123456, "busy_us": 7890123,
  "checks": 120, "check_errors": 0, "rx_resyncs": 0, "backlog_max": 9184, "proc_n": 4000, "proc_us": 240000, "copy_us": 110000}`.
  `mhz` = clock the start-up probe settled on (0 = no W5500); `half_duplex` =
  reads use the delay-compensated half-duplex path; `txn` and
  `busy_us` = SPI transactions and time spent in them since start (cumulative;
  sample twice to get rates). `checks`/`check_errors`: the version register
  is read every 500 ms as a link check; any error means the SPI clock is too
  fast for this board. `rx_resyncs`: received batches dropped because a frame
  length made no sense (also a sign of SPI errors). `backlog_max`: most bytes
  waiting in the W5500 at the start of a read since the last status request
  (its buffer holds 16 KB; near that, packets get dropped). `proc_n`,
  `proc_us`, `copy_us`: received packets handled, time spent handling them
  and, of that, copying into the LED frame (cumulative µs). `fifo`: register
  accesses go through the SPI FIFO (switched on after a start-up self-test,
  off for good after a transfer that did not end); `fifo_stuck`: such
  transfers; `bulk_stuck`: background chunk reads that did not end within
  5 ms and were cancelled. All counters are u32 and wrap (µs ones after
  ~71 min): use differences between two samples.
- `mdns`: `{"host": "artnetnode-423c.local"}` — the name the board announces.
- `led.memory`: the LED memory all outputs share: `bytes` (pool), `limit`
  (lower once the setup Wi-Fi took its share), `used`, `wifi_capped`,
  `effective_leds` (LEDs driven per output; less than configured when the
  outputs do not fit). Four frame copies live in the pool, so the outputs
  together get `limits.led_bytes` per frame (`limits.led_bytes_wifi` with
  the setup Wi-Fi on): 4156 RGBW / 5541 RGB LEDs, or 1596 RGBW with
  the setup Wi-Fi (it takes 40 KB of the pool for its ~47 KB of heap).
  Too many: shared evenly, what a short output leaves goes to the others.
- `heap_min`: lowest free heap seen.
- `stack_free_min`: bytes of the main stack never used since start (its
  deepest use so far; at least a few KB should stay free).
- `last_restart`: why the board last restarted: `kind` `power_on` | `planned`
  (restart, update, rollback; `message` says which) | `panic` (`message` with
  file:line, `backtrace` program counters: `xtensa-esp32-elf-addr2line -e
  <elf>`) | `watchdog` | `brownout` | `other` (`hw_reason` code);
  `uptime_s` before it; `unexpected_since_power_on`. Kept in RTC memory:
  survives restarts, not power cycles.
- `flash`: the flash chip and interface. `fast`: `off` | `qio80` (running at
  QIO 80 MHz) | `no_qe` (chip not in quad mode) | `verify_failed` (read-back
  at 80 MHz differed, stayed at DIO 40) | `after_crash` (a crash while fast;
  off until power-on). Also `id` (JEDEC), `sr`/`srh` (status registers),
  the SPI0 registers at boot and now (`now_ctrl`, `now_clock`).
- `test`: `null` or `{"pattern": "rainbow", "off_in_s": 1700}`.

### `GET /api/artnet?u=<universe>` (public)
Live view, poll once per second. `u` selects the universe whose channel
values are returned (0–32767; default 0).
```json
{
  "network": "eth", "window_ms": 1000, "pps": 640, "seq_gaps": 0, "sync": 40,
  "universes": [{"u": 0, "fps": 40}, {"u": 1, "fps": 40}],
  "watch": {"u": 0, "age_ms": 12, "data": "00ff7f..."}
}
```
- `led_type`: `"rgbw"` or `"rgb"` (the configured LED type, for the preview:
  128 RGBW or 170 RGB LEDs per universe).
- `universes`: universes 0–63 that received data in the last window, with
  frames per second. `u` is the 15-bit Art-Net port address
  (Net = u>>8, Sub-Net = (u>>4)&15, Universe = u&15).
- `watch.data`: the latest channel values of universe `watch.u` as a hex
  string, 2 characters per channel (up to 512 channels); `""` and
  `age_ms: null` if nothing was received for it yet.

### `GET /api/config` (login)
```json
{"name": "", "default_name": "ArtnetNode-423C", "eth": "dhcp", "ip": "2.0.0.10", "prefix": 8,
 "gateway": "", "fallback": true, "ap_channel": 6, "ap_after": 10, "ap_boot": 0, "ap_idle": 5,
 "default_password": true, "ota_available": true, "restart_allowed": true,
 "other_firmware": true, "other_reset": true, "other_back": "POST /pixelcast there, or three BOOT clicks", "slots": [
   {"slot": "ota_0", "running": false, "next": false, "kind": "other", "firmware": "WLED", "version": "17.0.0-devV5 ESP32_W5500", "built": null},
   {"slot": "ota_1", "running": true, "next": true, "kind": "this", "firmware": "esp32-pixelcast",
    "version": "0.1.0-1fee900", "built": "2026-11-04 11:27:00"}]}
```
`slots`: what each app slot held at start-up, in flash order. `kind`: `this`
(a build of this firmware), `other` (another firmware; `firmware` and
`version` from its adapter, e.g. `"WLED"` and WLED's version, else its
project name and version), `empty`
(no complete image), `changed` (an upload has started rewriting it; no
details). `built` only for this firmware. Uploads always go to the slot that
is not running.
Plus the LED settings:
```json
"led_type": "rgbw", "order": "grb", "brightness": 100,
"outputs": [{"leds": 512, "univ": 0, "type": "", "order": "", "rev": false, "group": 1, "offset": 0, "start": 1}, ...],
"t0h": 300, "t1h": 600, "period": 1200, "latch": 80, "protocol": "artnet",
"interp": false, "dither": false, "fast_flash": false, "merge": "htp",
"presets": [{"name": "SK6812 tuned", "period": 844, "t0h": 211, "t1h": 633, "latch": 100}], "loss": "hold", "loss_after": 5, "loss_color": "00000000",
"power_led_mw": 300, "power_out_mw": 0, "power_total_mw": 0, "led_volts_dv": 50,
"limits": {"max_leds": 4096, "max_samples_per_bit": 8, "max_sample_rate": 8000000}
```
- `name` "" means the default name (`default_name`), which is also the
  setup Wi-Fi name.
- `restart_allowed` false while new firmware is unconfirmed (updates and
  restarts are refused then; say "try again in a minute").
- `other_firmware` true when another firmware sits in the other app slot
  (see `../dualboot/README.md`); `other_reset` true when this firmware can
  reset it to its factory defaults (`POST /dualboot-reset`; WLED);
  `other_back` how to come back from it besides an upload in its update
  page (WLED: `"POST /pixelcast there, or three BOOT clicks"`), or `""`.
- `layout`: the partition table in flash: `dualboot` (`partitions.csv`, two
  1.5 MB slots), `old` (the previous two 1.9 MB slots; `POST
  /migrate-layout` moves it), `unknown`.
- `bootloader`: `bundled` when flash holds exactly the bootloader built into
  this firmware (`bootloader/`: rollback, BOOT button, in-place
  updates), `compatible` for another build of it at the same feature level
  or newer, else `other` (in-place updates need `bundled` or `compatible`).
- `in_place_note`: why the last in-place update gave up (text from the
  updater), or `null`.

### Actions (login + header `X-Requested-With: <anything>`)
All return `text/plain` with a human-readable message. 2xx = success, else
show the message as an error. 401 = login missing or changed: tell the user
to reload the page (the browser then asks for the login again).
- `POST /config`, body `application/x-www-form-urlencoded`: `name`,
  `password`, `password2` (empty password = keep), `eth` (`dhcp`|`static`),
  `ip`, `prefix` (1–30), `gateway` (may be empty), `fallback` (send `1` when
  enabled, omit when not; "omitted = off" only applies when `eth` is sent too), `fallback_ip`
  (the address used when DHCP gets no answer; empty = Art-Net's 2.x.y.z/8
  from the MAC), `fallback_prefix` (1–30, with `fallback_ip`), `ap_channel` (1–13), `ap_after` (5–3600 s), `ap_boot` (0–1440 min: the
  setup Wi-Fi runs this long after every start, 0 = no), `ap_idle` (1–240
  min: that start-up window ends only after this long without a web
  request over the setup Wi-Fi). While the setup Wi-Fi runs, and until the
  next restart, the LED memory is reduced (about 1600 RGBW LEDs in total);
  with `ap_boot` that is after every start.
  Applied live. If name/password/channel changed, devices on the setup Wi-Fi
  are disconnected (the reply may then never arrive; say so).
  LED fields (all optional; send only what the LED form contains):
  `led_type` (`rgb`|`rgbw`), `order` (permutation of r,g,b like `grb`; W is
  always last), `brightness` (1–100), `o1_leds`…`o4_leds` (0–4096, 0 = output
  off), `o1_univ`…`o4_univ` (0–32767, first universe of that output; each
  output uses consecutive universes, 128 RGBW or 170 RGB LEDs each),
  `t0h` (100–1000 ns), `t1h` (150–2000 ns), `period` (500–3000 ns),
  `latch` (50–1000 µs), `protocol` (`artnet`|`sacn`; sACN universe 1 feeds
  port-address 0), per output `oN_type` (`rgb`|`rgbw`|empty = global),
  `oN_order` (like `order`, empty = global), `oN_rev` (1|0), `oN_group` (1–64
  LEDs per pixel), `oN_offset` (0–4096 dark LEDs at the start); `interp` (1|0,
  blend between input frames at the full output rate), `dither` (1|0), `fast_flash` (1|0, applies after a restart), `merge` (`htp`|`ltp`), `presets` (`name|period|t0h|t1h|latch` joined by `;`, up to 8, names 1–24 plain characters without `| ; " \`; replaces the whole list), per output `oN_start` (DMX start address 1–512 in the first universe; following universes start at 1),
  `loss` (`hold`|`fade`|`color`), `loss_after` (1–3600 s), `loss_color`
  (`rrggbbww` hex), `power_led_mw` (1–50000 mW: one LED at full white; each
  output splits it over its 3 or 4 channels), `power_out_mw` (0–10000000 mW
  per output, 0 = no limit), `power_total_mw` (0–40000000 mW, all outputs:
  the supply), `led_volts_dv` (10–600, strip voltage in 0.1 V; only for the
  current shown). Rules: t0h < t1h and period ≥ t1h + 100.

### `GET /api/leds` (public)
`{"outputs": ["rrggbb…", "", …]}` — per output the received colours (after
grouping, offset and reverse; before brightness and the power limit),
sampled to at most 128 points, RGBW white mixed in. Empty string = output off.

### `GET /api/backup` (login)
All settings as `key=value&…` (the body `POST /config` takes), without the
password; served as a download (`Content-Disposition`).

### `POST /test` (login + `X-Requested-With`)
`pattern=off|rainbow|chase|solid`, for solid also `color=rrggbb` and
`white=0..255`. A pattern owns the outputs (Art-Net/sACN ignored) until
`off` or 30 minutes.

## Audio: the microphone as a DMX source
A microphone (or line-in ADC) on I2S0 is analysed on the board and sent as
one DMX universe, 44 times a second, also during silence. It does not
change the LED outputs: the console or software decides what to do with it.
Off by default.

Microphones (`au_mic`), sampled at 48 kHz, one slot:

| Type | For | Pins |
|---|---|---|
| `i2s` (default) | 24/32-bit I2S mics: SPH0645, INMP441, ICS-43434, ICS-43432 … | data, WS (LRCL), BCLK |
| `i2s_mclk` | the same plus a master clock (256 × 48 kHz) on GPIO 3, for line-in ADCs such as PCM1808 | as `i2s`, MCLK on GPIO 3 |
| `pdm` | PDM mics: SPM1423, MSM261 … | data, clock (on the WS pin) |

Pins are chosen from the board's free GPIOs: data from 2, 13, 15, 32, 33,
34, 35, 36, 39; WS/clock and BCLK from 2, 13, 15, 32, 33 (34–39 are
inputs only). Default: SPH0645 on data 32, WS 15, BCLK 33, SEL to GND.
`au_sel` is the mic's SEL (L/R) pin: `gnd` (left) or `vdd` (right). MCLK
can only be GPIO 0, 1 or 3 on the ESP32; 0 is BOOT and 1 the serial log
here, so it is 3 (the serial console loses its RX line). Type and pins apply
after a restart (`/api/config` `audio.mic_pending` until then); everything
else applies at once. Not supported: codecs that need an I2C setup (ES7243,
ES8388, WM8978) and analog mics.

Analysis: level (attack/release), three bands (low up to `au_low`, default
150 Hz; high above `au_high`, default 2 kHz; 12 dB/octave), onsets per
band, beat from the low band (60–200 BPM) with a phase ramp, activity,
clipping and mic faults. The bands rise within 2 ms (each hit shows) and
fall at the release. Values are shown in a window of `au_range` (12–48 dB,
default 24) below 0 dBFS after the gain. The AGC is a slow gain stage per value (level and each band, so
a quiet high band uses its range too): it follows the value's average over
about 10 s and sets its gain (−20 to +40 dB) so the average sits at 60 %
of the range; peaks go above it. It holds while the input is below
the gate and while it is 20 dB below its average (decays, breaks), unless
that lasts 5 s. Without AGC the gain is `au_gain`. The noise gate compares
the input before any gain; below it all values are 0.

Channel map (channels 1–16; 17–512 are 0):

| Ch | Value |
|---|---|
| 1–2 | level, 16-bit (coarse, fine) |
| 3 | level peak hold (falls over 1 s) |
| 4 | beat: 255 for 80 ms, else 0 |
| 5 | beat phase: 0→255 over each beat (runs on at the tempo for up to 4 beats, 0 without a tempo) |
| 6–8 | low, mid, high |
| 9–11 | onset low, mid, high: 255 for 80 ms |
| 12 | activity: 0 silence, 128 sound without a steady beat, 255 music |
| 13 | input level: 0 = −96 dBFS or less, 255 = 0 dBFS (before gain) |
| 14 | status: 64 mic fault (values 0), 128 below the gate, 192 active, 255 clipping |
| 15–16 | 0 (reserved) |

Sending: Art-Net (ArtDmx from UDP 6454; unicast to `au_dest`, else
broadcast on the board's network) or sACN (unicast, else multicast
239.255.x.y; priority `au_prio`, source name "<device name> audio"). The
universe is a port-address as everywhere here (sACN universe = it + 1) and
must not be one the LED outputs receive.

Settings (`POST /config`): `au_mic` (`i2s`|`i2s_mclk`|`pdm`), `au_sel`
(`gnd`|`vdd`), `au_din`, `au_ws`, `au_bclk` (GPIO numbers as above; after a
restart), `au_on` (send `1` when on), `au_proto`
(`artnet`|`sacn`), `au_dest` (IP or empty), `au_univ` (0–32767), `au_prio`
(1–200), `au_agc` (send `1` when on), `au_gain` (−20–40 dB, without AGC),
`au_gate` (−90 to −20 dBFS), `au_attack` (1–500 ms), `au_release`
(10–3000 ms), `au_sens` (beat sensitivity 1–10), `au_low` (40–500 Hz),
`au_high` (1000–8000 Hz), `au_range` (12–48 dB), `au_ctl` (send `1` when
on), `au_ctl_univ`, `au_ctl_addr` (control channels, below). A form with
`au_univ` but without `au_on`/`au_agc`/`au_ctl` turns those off. `/api/config` returns them as
`audio: {on, mic, sel, din, ws, bclk, mic_pending, proto, dest, univ,
prio, agc, gain, gate, attack, release, sens, low_hz, high_hz, range, ctl,
ctl_univ, ctl_addr}`.

- `GET /api/audio` (public): `{"on": true, "status":
  "fault|gated|active|clipping", "db": -42, "level": 0..65535, "peak",
  "beat", "phase", "beat_ms" (0: no tempo), "bands": [l, m, h], "onsets":
  [l, m, h], "activity": "silence|noise|music", "gain" (dB in effect for
  level, low, mid, high: the AGC's, or the manual gain four times), "control" (the console's control channels count now),
  "params": {agc, gain, gate, attack, release, sens, band_gain} (in effect,
  with the console's overrides), "restarts", "overruns", "max_gap_us"}`, or
  `{"on": false}`. `level`, `bands`, `onsets` and `beat` are the peak since
  the previous request (a poller then sees the hits between its requests).
  `restarts`: times the capture restarted after its DMA stopped (no data
  for 20 ms; should stay 0). `overruns`: times the capture task came back
  after the ring (10.7 ms; PDM 21 ms) had filled; the capture then restarts
  (a gap of that length). One at start-up; otherwise only under heavy HTTP
  load (e.g. 5 settings requests a second). `max_gap_us`: the longest the capture task was away since the
  previous request.
- `POST /audio-beat` (login + `X-Requested-With`): sends one beat (channels
  4 and 9 for 80 ms), to check a console's mapping without music.

Control channels (console → node, `au_ctl=1`): 11 DMX channels from
`au_ctl_addr` (1–502) in universe `au_ctl_univ` (a port-address; sACN
universe = it + 1), in the input protocol of the LED settings (Art-Net or
sACN). They override the settings live; a channel at 0 keeps the settings
page's value, and 2.5 s without data the settings apply again. The universe
must not be an LED universe nor the audio universe.

| Ch | Value (1–255) |
|---|---|
| 1 | AGC: 1–127 off, 128–255 on |
| 2 | gain without AGC: −20 to +40 dB |
| 3 | noise gate: −90 to −20 dBFS |
| 4 | attack: 1–500 ms |
| 5 | release: 10–3000 ms |
| 6 | beat sensitivity: 1–10 |
| 7–9 | gain low, mid, high: × value / 128 (128 = ×1, 255 ≈ ×2) |
| 10 | freeze: > 127 holds the output values |
| 11 | tap: each rise through 128 is a beat (the tempo follows the taps) |

Cost (measured, fast flash, alternating runs; table in README.md): 16
universes at 170 fps stay at 0 % loss with audio on; 36-universe bursts at
40 fps read 0.0–0.1 % with and 0 % without. Beyond the limits, at
48-universe bursts, loss rises by about two percentage points (1.9 % to
4.1–4.2 %).

## Art-Net / sACN behaviour
- ArtPoll → one ArtPollReply per group of up to four universes sharing
  Net and SubNet (BindIndex 1…n), unicast to the poller.
- ArtAddress → short name becomes the device name; Net/SubNet/SwOut changes
  apply to the ports of the given BindIndex. Outputs use consecutive
  universes, so moving a port moves its output's first universe; a universe
  shared by two outputs is one port, and moving it moves both. Applied at
  once, saved after a second without further changes, answered with
  ArtPollReply.
- Merge (Art-Net only): two sources per universe, told apart by IP; a third
  is ignored, a source silent for 10 s drops out. HTP (default) or LTP, from
  the settings or ArtAddress AcMergeLtp/AcMergeHtp (node-wide);
  AcCancelMerge keeps only the next source to send. ArtPollReply GoodOutputA
  shows merging (bit 3) and LTP (bit 1). `/api/status` `artnet.merging` =
  universes being merged.
- ArtSync: frames are shown on sync while syncs keep coming (2 s timeout).
- sACN (protocol `sacn`): UDP 5568, unicast and multicast 239.255.x.y for the
  configured universes; sACN universe = port-address + 1; highest priority
  wins per universe; preview data and terminated streams are ignored.
- mDNS: `<name>.local` answered on queries (multicast) and announced every 60 s
  (TTL 120 s).
- ArtPoll from a poller not seen before is answered twice (again after 300 ms).

## Measured limits (firmware with fast flash, W5500 at 80 MHz)
Unicast to the board's Ethernet address, rainbow pattern, 0 % loss over 12 s:
- Art-Net: about 2700 packets/s with 16 universes (170 fps), 3000 with 4
  universes; first sequence gaps from about 2880.
- sACN: about 2080 packets/s unicast (130 fps × 16); 0.1 % loss from 2400;
  multicast at 2400 still 0 %.
- Without fast flash: roughly 1500 (Art-Net) and 1150 (sACN) packets/s
  (estimates from earlier runs at DIO 40 MHz).
- LED core: no underruns with interpolation, dithering and the power limit
  together, at 844 ns (4 samples/bit, 72 fps, also with 30 fps in), 1000 ns
  with 8 samples/bit (8 MHz, 60 fps) and 500 ns (8 MHz, 120 fps). Rendering
  does not lower the input limit (2720 packets/s at 0 % with all on).
- The settings page warns when the universes at the full output rate need
  more than 80 % of these limits, and calls the setup likely unstable above
  100 % or with pulse times far outside the datasheet.
- Bursts: a sender that puts all universes on the wire back to back (every
  47 us at 100 Mbit/s) fills the W5500's 16 KB faster than 80 MHz SPI can
  empty it. The driver reads 4 KB chunks in the background while the frames
  already read are handled (Art-Net straight in the driver), and register
  accesses go through the SPI FIFO (~6 us instead of ~40 via esp-hal's DMA).
  Delivered: 36 universes 100 %, 42 99.9 %, 48 98.8 % (20 and 40 fps). The
  settings page warns above 48.
- Setup Wi-Fi started while 16 universes arrive at 72 fps: 0 % loss, one
  underrun at the start; it takes about 47 KB of heap.
- Not measured: two
  merging sources (HTP stores and compares both, a little more work per packet).

## LED timing calculator (must match the firmware, `firmware/core/src/led.rs` `quantize`)
One LED bit is `n` I2S samples; the sample rate is chosen so the bit time is
exact: `rate = floor(n * 1e9 / period)`. Pulses are rounded to whole samples:
```
for n in [4, 8]:                   // only these run at full speed
  rate = floor(n * 1e9 / period); if rate > 8_000_000: stop
  s = period * 1000 / n            // sample length in ps (integer division)
  h0 = clamp(round(t0h*1000 / s), 1, n-2)     // round = (x + s/2) / s, integer
  h1 = clamp(round(t1h*1000 / s), h0+1, n-1)
  worst = max(|h0*s - t0h*1000|, |h1*s - t1h*1000|)   // ps
  if worst <= 30_000: use this n (first that is good enough)
  else remember the n with the smallest worst
actual t0h = h0*s/1000, t1h = h1*s/1000 (ns, rounded), bit = period
max fps = floor(1e9 / (leds * channels * 8 * period + latch_us*1000))
data rate (kHz) = 1e6 / period
```
Datasheet tolerance for SK6812/WS2812: about ±150 ns on each high and low
time around T0H 300, T1H 600, T0L 900, T1L 600 (ns). Colour-code the four
actual times: within ±150 ns green, up to ±250 ns amber, beyond red. Presets:
Standard (300/600/1200, 4 samples/bit, ~50 fps at 512 RGBW), Fast in tolerance
(260/520/1040, 4 samples/bit, ~58 fps; all four times stay within ±150 ns), Custom.
Show samples/bit and the sample rate too: more samples per bit cost CPU.

## Firmware and maintenance actions (login + `X-Requested-With`)
- `POST /update`, body = the raw firmware `.bin` (use XMLHttpRequest for
  upload progress). On 200 the board restarts; the page should wait ~20 s
  and then go to `/`. Large file (~1 MB), takes ~15–30 s. This firmware
  installs on trial; another ESP32 firmware (e.g. WLED's `firmware.bin`) is written
  to the other slot, checked against the upload and its checksum, and started
  without a trial (this firmware does not run to confirm it).
- `POST /confirm` (empty body): keeps firmware that runs on trial after an
  update. Only an explicit confirm keeps it (working traffic does not); 409
  during its first 30 s. Without a confirm within 15 min, or after a crash
  or hang on trial, the board returns to the previous firmware.
- `POST /rollback` (empty body): returns to the previous firmware now (only
  on trial).
- `POST /restart` (empty body).
- `POST /debug/wifi` (empty body): starts the setup Wi-Fi as if BOOT were held
  (until the next restart; caps the LED memory).
- `POST /debug/panic` (empty body): crashes on purpose to test the crash log
  (refused while new firmware is unconfirmed).
- `POST /boot-other` (empty body): starts what the other slot holds, after a
  full image check. Another firmware: it gets the handoff (this firmware's
  address and LED setup, see `../dualboot/README.md`) and stays without a
  trial; way back: upload this firmware's `.bin` in its update page (the
  patched WLED: also `POST /pixelcast` there, or three BOOT clicks). Another
  build of this firmware (e.g. the previous version): it stays, without a
  trial (its own `/boot-other` or the BOOT button leads back). 409 when the
  slot is empty or damaged, while an upload rewrites it, or while new
  firmware is unconfirmed.
- `POST /migrate-layout` (empty body): moves a board with the old partition
  table to the dual-boot one without USB (settings carried over). Only
  when running confirmed from ota_0 with the expected old table; 409 with
  the reason otherwise. The table sector rewrite (~50 ms) must not lose power.
- `GET /api/dualboot-log` (login): the other firmware's log as text, from
  sector 3 of `storage` (the patched WLED writes its Ethernet driver log,
  network events and status at 15, 45, 120 and 280 s each time it runs);
  404 when there is none.
- `POST /dualboot-reset` (empty body): resets the other firmware to its
  factory defaults, if this firmware knows how (`other_reset`; WLED: erases
  the start of its filesystem); at its next start it takes this firmware's
  LED setup and address as its first configuration. 409 otherwise.
- `POST /update-in-place` (empty body): prepares an update of the running
  slot that keeps the other one (see `../updater/README.md`): writes the updater
  and a request into the end of the running slot, answers
  `{"token":"<32 hex digits>"}` and restarts into the updater. Then poll any
  GET until it answers `{"updater":true,...}` and `POST /update` the image
  with header `X-Update-Token: <token>` (no login there). 409 unless the
  bundled bootloader is installed, the running firmware is confirmed and
  selected, and Ethernet has an address.
- `POST /install-bootloader` (empty body): writes the bootloader built into
  this firmware (`bootloader/`) to 0x1000, reads it back, rewrites up to
  3 times. It takes effect at the next start. 409 when it is already there,
  while new firmware is unconfirmed, or when it did not read back (then do
  not restart: retry, or flash it over USB). The write (under a second) must
  not lose power: there is no second copy, only USB recovers.
- `POST /reset` (empty body): settings back to defaults, applied live.
