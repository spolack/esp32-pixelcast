# Bootloader: rollback + BOOT button

ESP-IDF v6.1's second stage bootloader with app rollback, plus the BOOT
button (`bootloader_components/main/bootloader_start.c`):

- `otadata` names the active slot. A slot selected as "new" boots once as
  "pending verify"; restarting before its firmware marks it valid makes it
  "aborted", and the other slot boots. An image that fails its checks boots
  the other slot too.
- After a power-on or EN reset the bootloader watches BOOT (GPIO0) for 2 s:
  pressed for 0.1 s, the other slot becomes active ("new"), if its image is
  intact. Press BOOT after the reset, not during it (held during reset the
  chip waits for USB flashing). Software restarts skip the wait.

- In-place update: a request record at the end of the active slot makes it
  load the updater stored there (`updater/`) into RAM and run it instead of
  the firmware; after the upload it checks the new image, selects it with
  the other slot as the fallback, and clears the record. Runs after the
  BOOT check, so BOOT is always the way out. Before the upload began (the
  firmware still intact), a power-on or EN reset cancels the update.

Logs at warning level (room in the 28 KB region); its own messages (BOOT
window, slot switches, in-place steps) are warnings, so they still show.

Build: `tools/build.sh bootloader` (`dist/bootloader.bin`), with a local
ESP-IDF v6.1 or else in the `espressif/idf:v6.1` image through podman or
docker. The build is reproducible: the same sources give the same bytes
locally and in CI. By hand: `idf.py bootloader` in this directory after
sourcing ESP-IDF's `export.sh`.

The firmware carries this bootloader to install it over the network;
`tools/build.sh firmware` and `tools/cargo.sh` rebuild it when its sources
changed and embed it.

Install over the network: on the running firmware, `POST /install-bootloader`
(see `docs/api.md`); `/api/config` then shows `"bootloader": "bundled"`. Or
over USB: `espflash write-bin 0x1000 dist/bootloader.bin`. There is no second
copy: a power loss during the write (under a second) needs USB to recover.

Flash at 40 MHz DIO: only the bootloader's own reads (~0.1 s) use it; each
firmware sets its own speed afterwards (this firmware's "fast flash"
setting: QIO 80 MHz after a check). A bootloader set too fast for the flash
chip can only be fixed over USB.

QEMU tests: build with `-D SDKCONFIG_DEFAULTS="sdkconfig.defaults;sdkconfig.qemu-nobutton"`
(QEMU reads GPIO0 as pressed) and boot a 4 MB image with
`qemu-system-xtensa -machine esp32 -drive file=flash.bin,if=mtd,format=raw`.
