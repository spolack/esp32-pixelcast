# Open points

## Needs someone at the board
- **BOOT button check:** click cycles rainbow → chase → blackout → off; 3 s: blue, setup
  Wi-Fi; 10 s: blinking red, password and network back to defaults (DHCP).
- **Status LED stays dark** on the V2.2 board under both this firmware and WLED,
  although GPIO 16 carries a clean WS2812 frame (checked with a scope under WLED).
  Left to check: the frame at the LED's DIN, the LED's 5 V supply, the LED itself.
  Then the colours: green when ready, green with a short white flash twice a second
  while Art-Net/sACN arrives, purple during a test pattern, blinking purple during an
  update, blue with only the setup Wi-Fi, blinking red without any network.

## To investigate
- Bursts: 48 universes back to back arrive at 98.1–98.8 % (36 at 100 %). The remaining
  loss: 80 MHz SPI (~10 MB/s) cannot keep up with 100 Mbit/s (12.2 MB/s) for long.
  Left: 2 register accesses per chunk for the release, 4 KB chunks (DMA buffer).
  Diagnostics: status spi (fifo, fifo_stuck, bulk_stuck, backlog_max).
- Main stack: 24 952 bytes, of which the linker check requires 24 576; the
  deepest use measured leaves ~3.9 KB free (`stack_free_min`). New static
  buffers come out of it, so there is almost no room left: use the heap
  (~15 KB free) for buffers allocated once.
- Async SPI (esp-hal 1.2.2, tried and reverted): short transfers through the FIFO cost
  ~25 us instead of ~34-46, but some DMA writes (130 bytes, also when padded to 132)
  never complete; with a 3 ms time limit and a blocking retry it worked, yet bursts
  and 72 fps got worse than blocking (36: 96-98 %, 48: 80-86 %). The 4-byte length
  theory for the old hang is ruled out for writes. Blocking stays.
- LED output: esp-hal #6154 (true circular DMA buffers) would let a frame start before
  its ring is full.
- One unexplained restart while 300 mDNS queries per second were sent to 224.0.0.251
  (uptime reset); not reproduced in four later floods (unicast, multicast, closed port).
- mDNS: a listening responder runs now (multicast answers within ~2 ms, announcements
  every 60 s; 72 fps stays at 0 % loss under 100 queries/s). Legacy unicast replies
  to queries from ports other than 5353 do not arrive (and would need the query ID
  echoed); avahi, Bonjour and Windows use multicast.
- First ArtPoll after a restart: fixed by answering a new poller a second time 300 ms
  later (the first reply can be dropped while the stack resolves its MAC).
- Occasional HTTP stalls seen early on (before the dual-core work); not seen since.
- clippy `large_stack_frames` errors (22, existed before this work).

## From the pre-publication review (not done yet)
- Updates: the image hash check (~4.5 MB read) runs with the LED core paused, freezing
  the LEDs for seconds (`firmware/src/ota.rs` around `finish`); verify unpaused, pause only for
  the record and otadata writes.
- `firmware/src/ledout.rs` transfer loop has no time bound: a DMA that never reaches EOF spins
  while the watchdog keeps getting fed. Bound it by the frame time and rebuild.
- `firmware/core/src/merge.rs`: infallible `push` on the per-packet path can hit OOM with the
  setup Wi-Fi up and many universes; use `try_reserve`.
- `clippy::large_stack_frames` only denied in `main.rs`; move to `[lints.clippy]` and
  paint the core 1 stack like `stackwatch.rs`.
- ArtAddress renames postpone the settings save while they keep coming (`main.rs`
  save debounce); cap the delay.
- macOS: `stat -c` (updater/build.sh), `sha256sum` and `realpath` (tools/build.sh).
- Wiznet patch: split and feature-gate before ever sending it upstream.
- Smaller: HTTP rejects no `Transfer-Encoding`; login has no rate limit; the updater
  checks no chip id before erasing; `core1::park` relies on inlining into the `#[ram]`
  function (check once with objdump); `dualboot/README.md` "Moving a board to this
  layout" and `/migrate-layout` serve a pre-public layout and can go.
- Status LED dark on the V2.2 board (see above); BOOT switch since the bootloader now
  selects "valid": check once at the board.

## Audio (next steps)
- OSC out (`/pixelcast/level`, `/beat`, `/bands`, `/status`).
- Fixture profiles for the audio universe and the control channels (GDTF
  for grandMA3, MagicQ, QLC+).
- Settings page: band gains (now only from the control channels).
- BPM and tempo confidence on channels 15–16.
- Advertise the audio universe as an input port in ArtPollReply.
- Audio capture: under heavy HTTP load (5 settings requests a second) the
  network core sometimes keeps the capture task away up to ~11 ms, a little
  longer than the 10.7 ms ring: an overrun (none with the settings page's
  meter polling); a settings save does the same (flash write). Each overrun
  restarts the capture, a gap of ~11 ms in the audio. Also: the "available"
  interrupt does not always wake the task (a 3 ms timeout covers it). A
  larger ring needs RAM (see Main stack above).
  Knobs: the receive task's buffers (`rx.rs`), the two heap regions
  (`main.rs`), the LED pool (`ledout.rs`, reclaimed RAM, 2.5 KB left there).
- Mics: PDM and I2S with MCLK are built but untested on hardware (no such
  mic here); SEL to 3.3 V is inferred from the tested GND case. Codecs that
  need an I2C setup (ES7243, ES8388, WM8978; would share the OLED's bus)
  and analog mics are not supported.
- Art-Net broadcast and sACN multicast of the audio universe: built, not yet
  received on a test machine (its firewall drops them).
