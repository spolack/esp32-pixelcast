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
- Bursts: 48 universes back to back arrive at ~99 % (36 at 100 %). The remaining
  loss: 80 MHz SPI (~10 MB/s) cannot keep up with 100 Mbit/s (12.2 MB/s) for long.
  Left: 2 register accesses per chunk for the release, 4 KB chunks (DMA buffer).
  Diagnostics: status spi (fifo, fifo_stuck, bulk_stuck, backlog_max).
- Main stack: measured peak ~19.5 KB of 27.7 KB (`stack_free_min`); the link
  check is 24 KB. Spare RAM for new static buffers comes from there.
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
