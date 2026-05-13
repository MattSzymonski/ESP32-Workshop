// Trampoline that exposes the IDF C macro `BT_CONTROLLER_INIT_CONFIG_DEFAULT()`
// to Rust.
//
// NOTE: this function is currently unused — `nimble_port_init()` already
// performs `esp_bt_controller_init()` (with the macro-generated default
// config) and `esp_bt_controller_enable(ESP_BT_MODE_BLE)` internally, so
// there is no need to call them again from Rust. The component is kept
// because removing it would force a multi-minute full IDF rebuild and the
// trampoline costs <100 bytes; it may be needed again if we ever switch to
// manual controller bring-up (e.g. to override `ble_ll_tx_pwr_dbm`).

#include <string.h>
#include "esp_bt.h"

void rust_gamepad_bt_default_cfg(esp_bt_controller_config_t *out)
{
    esp_bt_controller_config_t tmp = BT_CONTROLLER_INIT_CONFIG_DEFAULT();
    memcpy(out, &tmp, sizeof(tmp));
}
