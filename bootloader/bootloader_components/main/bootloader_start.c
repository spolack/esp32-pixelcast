/*
 * esp32-pixelcast bootloader: ESP-IDF's second stage bootloader with app
 * rollback, plus the BOOT button to switch app slots.
 *
 * - otadata names the active slot (ota_0/ota_1). With rollback, a slot that
 *   was selected as "new" boots once as "pending verify"; if the board
 *   restarts before its firmware marks it valid, it is marked aborted and the
 *   other slot boots. An image that fails its checks boots the other slot too.
 * - BOOT button: after a power-on or EN reset, this bootloader watches BOOT
 *   (GPIO0) for 2 s. Pressed for at least 0.1 s: the other slot becomes the
 *   active one ("new", so rollback covers it), if it holds an intact image.
 *   BOOT must be pressed after the reset, not during it (held during reset
 *   the chip waits for USB flashing). Software restarts skip the wait.
 * - In-place update: a record at the end of the active slot (see
 *   firmware/core/src/inplace.rs) makes this bootloader start the updater stored
 *   next to it (a RAM-only app) instead of the firmware; after the upload it
 *   checks the new image and selects it. Runs after the BOOT button check,
 *   so BOOT is always the way out. Before the upload began (firmware still
 *   intact), a power-on or EN reset cancels the update.
 *
 * Based on ESP-IDF's examples/custom_bootloader/bootloader_override.
 * SPDX-License-Identifier: Apache-2.0
 */
#include <stdbool.h>
#include <string.h>
#include <sys/reent.h>
#include "sdkconfig.h"
#include "esp_log.h"
#include "esp_rom_sys.h"
#include "esp_rom_gpio.h"
#include "bootloader_init.h"
#include "bootloader_utility.h"
#include "bootloader_common.h"
#include "bootloader_flash_priv.h"
#include "esp_flash_partitions.h"
#include "esp_image_format.h"
#include "hal/gpio_ll.h"
#include "soc/gpio_struct.h"
#include "soc/reset_reasons.h"

static const char *TAG = "boot";

/* Feature level, found by the firmware in the installed bootloader
 * (firmware/src/ota.rs): 1 rollback and the BOOT slot switch, 2 the in-place update
 * hook (with power-on cancel). Raise it when the firmware comes to depend
 * on something new here. */
static const char BOOT_ID[] = "pixelcast-bootloader 2";

#define BUTTON_GPIO 0
#define WINDOW_MS 2000
#define PRESS_MS 100
#define STEP_MS 10

static bool button_pressed_in_window(void)
{
    esp_rom_gpio_pad_select_gpio(BUTTON_GPIO);
    gpio_ll_input_enable(&GPIO, BUTTON_GPIO);
    ESP_LOGW(TAG, "BOOT within %d s switches the app slot", WINDOW_MS / 1000);
    int low_ms = 0;
    for (int t = 0; t < WINDOW_MS; t += STEP_MS) {
        if (gpio_ll_get_level(&GPIO, BUTTON_GPIO) == 0) {
            low_ms += STEP_MS;
            if (low_ms >= PRESS_MS) {
                return true;
            }
        } else {
            low_ms = 0;
        }
        esp_rom_delay_us(STEP_MS * 1000);
    }
    return false;
}

static bool write_otadata(const bootloader_state_t *bs, int sector, uint32_t seq, uint32_t state)
{
    esp_ota_select_entry_t e;
    memset(&e, 0xFF, sizeof(e));
    e.ota_seq = seq;
    e.ota_state = state;
    e.crc = bootloader_common_ota_select_crc(&e);
    uint32_t addr = bs->ota_info.offset + sector * FLASH_SECTOR_SIZE;
    if (bootloader_flash_erase_sector(addr / FLASH_SECTOR_SIZE) != ESP_OK ||
            bootloader_flash_write(addr, &e, sizeof(e), false) != ESP_OK) {
        ESP_LOGE(TAG, "writing otadata failed");
        return false;
    }
    return true;
}

/* Makes `slot` the active one in otadata with `state`: "new" for an image
 * that must confirm itself (in-place update), "valid" for a switch someone
 * at the board asked for (BOOT button). Writes the sector of the entry that
 * is not active, so the current entry stays as the way back for rollback.
 * Returns the new entry's sequence number (0: failed). */
static uint32_t select_slot(const bootloader_state_t *bs, esp_ota_select_entry_t otadata[2], int slot,
                            uint32_t state)
{
    int active = bootloader_common_get_active_otadata(otadata);
    uint32_t seq = 0;
    for (int i = 0; i < 2; i++) {
        if (bootloader_common_ota_select_valid(&otadata[i]) && otadata[i].ota_seq > seq) {
            seq = otadata[i].ota_seq;
        }
    }
    seq += 1;
    if ((seq - 1) % bs->app_count != (uint32_t)slot) {
        seq += 1;
    }
    int sector = active == 0 ? 1 : 0;
    if (!write_otadata(bs, sector, seq, state)) {
        return 0;
    }
    ESP_LOGW(TAG, "ota_%d selected (seq %lu)", slot, (unsigned long)seq);
    return seq;
}

/* The slot the stock selection would boot (without changing anything). */
static int current_slot(const bootloader_state_t *bs, esp_ota_select_entry_t otadata[2])
{
    int active = bootloader_common_get_active_otadata(otadata);
    if (active < 0) {
        return 0;  /* empty otadata: ota_0 */
    }
    return (otadata[active].ota_seq - 1) % bs->app_count;
}

/* ── In-place update (layout and record: firmware/core/src/inplace.rs) ── */

#define PXCU_UPDATER    0x177000
#define PXCU_UPDATER_MAX 0x8000
#define PXCU_RECORD     0x17F000
#define PXCU_LEN        48
#define PXCU_START      1
#define PXCU_WRITING    2
#define PXCU_WRITTEN    3
/* Updater starts while the old firmware is still intact before giving up
 * (an updater that cannot reach the network restarts via the watchdog). */
#define PXCU_MAX_STARTS 5

static uint32_t pxcu_crc32(const uint8_t *d, size_t n)
{
    uint32_t crc = 0xFFFFFFFF;
    for (size_t i = 0; i < n; i++) {
        crc ^= d[i];
        for (int b = 0; b < 8; b++) {
            crc = (crc & 1) ? (crc >> 1) ^ 0xEDB88320 : crc >> 1;
        }
    }
    return ~crc;
}

static bool pxcu_read(uint32_t at, uint8_t r[PXCU_LEN])
{
    if (bootloader_flash_read(at, r, PXCU_LEN, true) != ESP_OK) {
        return false;
    }
    uint32_t crc = r[44] | (r[45] << 8) | (r[46] << 16) | ((uint32_t)r[47] << 24);
    return memcmp(r, "PXCU", 4) == 0 && r[4] == 1 && r[5] >= PXCU_START && r[5] <= PXCU_WRITTEN
           && crc == pxcu_crc32(r, 44);
}

static void pxcu_write(uint32_t at, uint8_t r[PXCU_LEN])
{
    uint32_t crc = pxcu_crc32(r, 44);
    memcpy(&r[44], &crc, 4);
    if (bootloader_flash_erase_sector(at / FLASH_SECTOR_SIZE) != ESP_OK ||
            bootloader_flash_write(at, r, PXCU_LEN, false) != ESP_OK) {
        ESP_LOGE(TAG, "in-place update: writing the record failed");
    }
}

static void pxcu_clear(uint32_t at)
{
    if (bootloader_flash_erase_sector(at / FLASH_SECTOR_SIZE) != ESP_OK) {
        ESP_LOGE(TAG, "in-place update: erasing the record failed");
    }
}

/* Loads the updater into RAM and runs it; returns only if it is damaged. */
static void pxcu_run_updater(uint32_t slot_offset)
{
    esp_partition_pos_t pos = { .offset = slot_offset + PXCU_UPDATER, .size = PXCU_UPDATER_MAX };
    esp_image_metadata_t data;
    if (bootloader_load_image(&pos, &data) != ESP_OK) {
        ESP_LOGE(TAG, "in-place update: the updater image is damaged");
        return;
    }
    ESP_LOGW(TAG, "in-place update: starting the updater");
    ((void (*)(uint32_t))data.image.entry_addr)(slot_offset);
    bootloader_reset();
}

static void inplace_update(const bootloader_state_t *bs)
{
    if (bs->ota_info.offset == 0 || bs->app_count != 2) {
        return;
    }
    esp_ota_select_entry_t otadata[2];
    if (bootloader_common_read_otadata(&bs->ota_info, otadata) != ESP_OK) {
        return;
    }
    int slot = current_slot(bs, otadata);
    const esp_partition_pos_t *part = &bs->ota[slot];
    uint32_t at = part->offset + PXCU_RECORD;
    uint8_t r[PXCU_LEN];
    if (!pxcu_read(at, r)) {
        return;
    }
    esp_image_metadata_t data;
    if (r[5] == PXCU_START) {
        /* Power-cycle or EN before the upload: someone at the board wants
         * out. The firmware is still intact. */
        if (esp_rom_get_reset_reason(0) == RESET_REASON_CHIP_POWER_ON) {
            ESP_LOGW(TAG, "in-place update: cancelled by power-on/EN");
            pxcu_clear(at);
            return;
        }
        if (r[6] >= PXCU_MAX_STARTS && esp_image_verify(ESP_IMAGE_VERIFY_SILENT, part, &data) == ESP_OK) {
            ESP_LOGW(TAG, "in-place update: the updater started %d times without an upload; keeping the firmware", r[6]);
            pxcu_clear(at);
            return;
        }
        r[6]++;
        pxcu_write(at, r);
        pxcu_run_updater(part->offset);
        /* Damaged updater, firmware untouched: drop the request. */
        pxcu_clear(at);
        return;
    }
    /* Writing or written: done if the new image checks out. */
    if (esp_image_verify(ESP_IMAGE_VERIFY_SILENT, part, &data) == ESP_OK) {
        ESP_LOGW(TAG, "in-place update: new firmware in ota_%d checks out; selecting it", slot);
        int sector = bootloader_common_get_active_otadata(otadata) == 0 ? 1 : 0;
        uint32_t seq = select_slot(bs, otadata, slot, ESP_OTA_IMG_NEW);
        /* The entry left behind names this same slot (the old firmware is
         * gone): point it at the other slot, so a new firmware that fails
         * before confirming itself falls back there. Only if that one is
         * intact. A power cut before this just keeps the same-slot entry. */
        const esp_partition_pos_t *other = &bs->ota[1 - slot];
        if (seq > 1 && esp_image_verify(ESP_IMAGE_VERIFY_SILENT, other, &data) == ESP_OK) {
            write_otadata(bs, 1 - sector, seq - 1, ESP_OTA_IMG_VALID);
        }
        pxcu_clear(at);
        return;
    }
    pxcu_run_updater(part->offset);
    /* Damaged updater and no firmware here: the usual fallback to the
     * other slot follows. */
}

static void button_switch(const bootloader_state_t *bs)
{
#ifndef CONFIG_PXC_BOOT_BUTTON
    return;
#endif
    if (bs->ota_info.offset == 0 || bs->app_count != 2) {
        return;
    }
    if (esp_rom_get_reset_reason(0) != RESET_REASON_CHIP_POWER_ON) {
        return;
    }
    if (!button_pressed_in_window()) {
        return;
    }
    esp_ota_select_entry_t otadata[2];
    if (bootloader_common_read_otadata(&bs->ota_info, otadata) != ESP_OK) {
        return;
    }
    int target = 1 - current_slot(bs, otadata);
    esp_image_metadata_t data;
    if (esp_image_verify(ESP_IMAGE_VERIFY_SILENT, &bs->ota[target], &data) != ESP_OK) {
        ESP_LOGW(TAG, "BOOT pressed, but ota_%d holds no intact image: not switched", target);
        return;
    }
    /* Valid: no trial for a switch made at the board (a trial would send
     * the board back to the other slot at the next restart). */
    select_slot(bs, otadata, target, ESP_OTA_IMG_VALID);
}

void __attribute__((noreturn)) call_start_cpu0(void)
{
    if (bootloader_init() != ESP_OK) {
        bootloader_reset();
    }
    /* Also keeps the marker in the image (the linker drops unused data). */
    ESP_LOGW(TAG, "%s", BOOT_ID);

#ifdef CONFIG_BOOTLOADER_SKIP_VALIDATE_IN_DEEP_SLEEP
    bootloader_utility_load_boot_image_from_deep_sleep();
#endif

    bootloader_state_t bs = {0};
    if (!bootloader_utility_load_partition_table(&bs)) {
        ESP_LOGE(TAG, "load partition table error!");
        bootloader_reset();
    }
    button_switch(&bs);
    inplace_update(&bs);

    int boot_index = bootloader_utility_get_selected_boot_partition(&bs);
    if (boot_index == INVALID_INDEX) {
        bootloader_reset();
    }
    bootloader_utility_load_boot_image(&bs, boot_index);
}

#if CONFIG_LIBC_NEWLIB
struct _reent *__getreent(void)
{
    return _GLOBAL_REENT;
}
#endif
