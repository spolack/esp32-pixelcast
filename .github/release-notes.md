Images for ESP32 boards with a W5500 (4 MB flash; see README.md).

| File | What | Where |
|---|---|---|
| `full-4MB.bin` | everything below in one image, for the first flash over USB | `espflash write-bin 0x0 full-4MB.bin` |
| `esp32-pixelcast.bin` | the firmware | settings page → Firmware update |
| `wled.bin` | WLED (upstream main + patches/wled) for the other slot | settings page → Firmware update |
| `bootloader.bin` | bootloader (rollback, BOOT slot switch, in-place updates) | 0x1000; the firmware also installs it over the network |
| `partitions.bin` | partition table | 0x8000 |
| `updater.bin` | in-place updater (also built into the firmware) | — |

Flashing `full-4MB.bin` erases the settings. `SHA256SUMS` lists the checksums.
