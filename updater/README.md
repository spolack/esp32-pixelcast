# In-place updater

Replaces the running firmware without touching the other slot (e.g. WLED).
The whole picture and the record format: `core/src/inplace.rs`.

1. `POST /update-in-place` (settings page: "Install into this slot") writes
   this updater and a request record into the unused end of the running slot
   (`0x177000` and `0x17F000` within the 1.5 MB slot) and restarts. The
   response carries a one-time token.
2. The bootloader (`tools/bootloader`) finds the record, loads the updater
   into RAM and starts it. The updater runs from RAM only, so it can rewrite
   the slot it came from. It brings the W5500 up in its hardware TCP mode at
   the board's address (handed over in the record) and answers any GET with
   `{"updater":true,...}`.
3. `POST /update` with `X-Update-Token: <token>` and the image as body: the
   updater marks the record "writing", writes the image from the start of
   the slot, marks it "written" and restarts.
4. The bootloader checks the image, selects it ("new": it must confirm itself)
   with the other slot as the fallback, clears the record, and boots it.

Failure handling:
- No upload within 10 minutes (slot untouched): the updater clears the record
  and leaves a note (shown on the settings page); the old firmware starts.
- The updater restarting before an upload (5 times): the bootloader drops the
  request.
- Upload interrupted after writing began (old firmware gone): every start goes
  back into the updater, which waits for the file. BOOT within 2 s of a
  power-on starts the other slot instead.
- New firmware fails before confirming itself: the other slot starts.

No serial output is needed, but the updater logs (`updater: ...`) on UART0.

Build: `tools/build.sh updater` (dist/updater.bin, which `tools/build.sh
firmware` embeds) or `./build.sh` here (writes the prebuilt `updater.bin` a
plain `cargo build` embeds). Size: about 9 KB (budget 32 KB). It drives the
SPI3 controller directly (4 MHz) and links IDF's patched flash routines from
esp-rom-sys (the ESP32 ROM's own write routine corrupts data).
