// This file implements the gamepad module: a real BLE Central / GATT client
// that talks to a BLE HID gamepad (8BitDo Ultimate 2C in BLE / Switch mode)
// directly on top of NimBLE — no `esp_hidh` managed component required.
//
// Pipeline (all callbacks run on the NimBLE host task):
//   1. `bt_stack_init`            — bring up BT controller + NimBLE host port
//   2. `host_task`                — runs `nimble_port_run()` (NimBLE event loop)
//   3. `on_sync`                  — auto-pick our address, then start scanning
//   4. `gap_event_scan`           — for each adv report, parse 16-bit UUIDs;
//                                   if 0x1812 (HID Service) is in the list,
//                                   cancel scan and `ble_gap_connect`
//   5. `gap_event_conn` (CONNECT) — discover all characteristics on the link
//   6. `chr_disc_cb`              — for every characteristic with the NOTIFY
//                                   property, write 0x0001 to its CCCD
//                                   (the descriptor immediately after
//                                   `val_handle` per the HID Service spec)
//   7. `gap_event_conn` (NOTIFY_RX) — copy mbuf into a local buffer and feed
//                                     it to `decode_report`, which updates
//                                     the shared `GamepadState`.
//
// HTTP endpoints (registered on the EspHttpServer):
//   GET /api/gamepad             — full state JSON snapshot
//   GET /api/gamepad/scan        — (re)start a scan from the UI
//   GET /api/gamepad/disconnect  — terminate the current connection
//
// All FFI is `unsafe` because the NimBLE C API is. The signatures used here
// come from the `esp-idf-sys` bindings emitted under
// CONFIG_BT_NIMBLE_ENABLED=y (see sdkconfig.defaults).

use esp_idf_svc::http::server::EspHttpServer;
use esp_idf_svc::sys::*;
use log_crate::{info, warn};
use std::sync::{Arc, Mutex};

const CARD_HTML: &str = include_str!("card.html");

// Number of buttons / axes the UI displays. Reports with fewer slots simply
// leave the unused entries at zero.
const NUM_BUTTONS: usize = 17;
const NUM_AXES: usize = 4;

// Standard 16-bit HID Service UUID (Bluetooth SIG-assigned).
const HID_SERVICE_UUID16: u16 = 0x1812;

// `BLE_HS_FOREVER` is a C macro and not in the auto-generated bindings.
// Its value is INT32_MAX (the duration is in ms; this means "never expire").
const BLE_HS_FOREVER: i32 = i32::MAX;

// CCCD payload to enable notifications.
const CCCD_NOTIFY: [u8; 2] = [0x01, 0x00];

// HID Service "Report" characteristic UUID — used so we can log which
// subscriptions are the meaningful ones (vs. e.g. battery level).
const HID_REPORT_UUID16: u16 = 0x2A4D;

/// Snapshot of the current gamepad state, serialised to JSON for the UI.
#[derive(Clone, Default)]
struct GamepadState {
    /// Always 0 — single device support.
    index: u32,
    /// True between successful CONNECT and DISCONNECT events.
    connected: bool,
    /// Always "standard" — matches the Web Gamepad API "standard" mapping.
    mapping: &'static str,
    /// Seconds since boot at the moment the last report was received.
    timestamp: f64,
    /// Reserved — vibration not wired up.
    vibration: bool,
    /// Button values in 0..=1.0 (binary buttons report 0.0 or 1.0).
    buttons: [f32; NUM_BUTTONS],
    /// Axes in -1.0..=1.0.
    axes: [f32; NUM_AXES],
}

impl GamepadState {
    fn to_json(&self) -> String {
        let mut s = String::with_capacity(256);
        s.push('{');
        s.push_str(&format!("\"index\":{},", self.index));
        s.push_str(&format!("\"connected\":{},", self.connected));
        s.push_str(&format!("\"mapping\":\"{}\",", self.mapping));
        s.push_str(&format!("\"timestamp\":{:.5},", self.timestamp));
        s.push_str(&format!("\"vibration\":{},", self.vibration));
        s.push_str("\"buttons\":[");
        for (i, b) in self.buttons.iter().enumerate() {
            if i > 0 {
                s.push(',');
            }
            s.push_str(&format!("{:.3}", b));
        }
        s.push_str("],\"axes\":[");
        for (i, a) in self.axes.iter().enumerate() {
            if i > 0 {
                s.push(',');
            }
            s.push_str(&format!("{:.5}", a));
        }
        s.push(']');
        s.push('}');
        s
    }
}

type SharedState = Arc<Mutex<GamepadState>>;

// ---------------------------------------------------------------------------
// Globals — the NimBLE C callbacks cannot capture environment, so the shared
// state and the current connection handle live in module-level statics.
// ---------------------------------------------------------------------------

/// Set once at module init. Read by every callback via `global_state()`.
static mut GLOBAL_STATE: Option<SharedState> = None;

fn global_state() -> Option<&'static SharedState> {
    // SAFETY: written exactly once before any reader runs.
    unsafe { (*core::ptr::addr_of!(GLOBAL_STATE)).as_ref() }
}

/// Connection handle of the currently-open peer; `0xFFFF` means "none".
/// All access is from the NimBLE host task → no atomic needed, but we use
/// raw-pointer reads to avoid `static_mut_refs` lints.
static mut CONN_HANDLE: u16 = 0xFFFF;

fn conn_handle_get() -> u16 {
    unsafe { core::ptr::read(core::ptr::addr_of!(CONN_HANDLE)) }
}

fn conn_handle_set(h: u16) {
    unsafe { core::ptr::write(core::ptr::addr_of_mut!(CONN_HANDLE), h) }
}

/// Stored after `on_sync` and re-used by every `start_scan` call.
static mut OWN_ADDR_TYPE: u8 = 0;

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Registers the gamepad endpoints on the server, brings up the BLE stack,
/// and returns the HTML card to embed on the main page.
pub fn register(server: &mut EspHttpServer) -> anyhow::Result<String> {
    // 1. Build shared state and stash a clone in the global slot for the
    //    NimBLE callbacks.
    let state: SharedState = Arc::new(Mutex::new(GamepadState {
        mapping: "standard",
        ..Default::default()
    }));
    unsafe {
        GLOBAL_STATE = Some(state.clone());
    }

    // 2. Bring up the BT controller + NimBLE on a dedicated thread.
    //    `nimble_port_run()` blocks forever dispatching events, so it lives
    //    on its own Rust thread.
    std::thread::Builder::new()
        .stack_size(4096)
        .name("ble-host".into())
        .spawn(|| unsafe {
            if let Err(e) = bt_stack_init() {
                warn!("gamepad: BT stack init failed: {:?}", e);
                return;
            }
            info!("gamepad: NimBLE host task running");
            // Blocks forever, dispatching NimBLE events.
            nimble_port_run();
        })
        .expect("gamepad: failed to spawn BLE host thread");

    // 3. HTTP: GET /api/gamepad — full state JSON.
    let state_for_read = state.clone();
    server.fn_handler("/api/gamepad", esp_idf_svc::http::Method::Get, move |req| {
        let snapshot = state_for_read.lock().unwrap().clone();
        let json = snapshot.to_json();
        let mut response =
            req.into_response(200, Some("OK"), &[("Content-Type", "application/json")])?;
        response.write(json.as_bytes())?;
        Ok::<(), anyhow::Error>(())
    })?;

    // 4. HTTP: GET /api/gamepad/scan — request a (re)scan.
    server.fn_handler(
        "/api/gamepad/scan",
        esp_idf_svc::http::Method::Get,
        move |req| {
            let started = unsafe { start_scan() };
            let body: &[u8] = if started {
                br#"{"message":"Scan started"}"#
            } else {
                br#"{"message":"Scan request failed (already scanning or BT not ready)"}"#
            };
            let mut response =
                req.into_response(200, Some("OK"), &[("Content-Type", "application/json")])?;
            response.write(body)?;
            Ok::<(), anyhow::Error>(())
        },
    )?;

    // 5. HTTP: GET /api/gamepad/disconnect — terminate current connection.
    server.fn_handler(
        "/api/gamepad/disconnect",
        esp_idf_svc::http::Method::Get,
        move |req| {
            let h = conn_handle_get();
            let body: &[u8] = if h != 0xFFFF {
                unsafe {
                    // 0x13 = remote user terminated connection (HCI reason).
                    ble_gap_terminate(h, 0x13);
                }
                br#"{"message":"Disconnect requested"}"#
            } else {
                br#"{"message":"No connection"}"#
            };
            let mut response =
                req.into_response(200, Some("OK"), &[("Content-Type", "application/json")])?;
            response.write(body)?;
            Ok::<(), anyhow::Error>(())
        },
    )?;

    info!("gamepad: registered /api/gamepad, /api/gamepad/scan, /api/gamepad/disconnect");
    Ok(CARD_HTML.to_string())
}

// ===========================================================================
// BT bring-up
// ===========================================================================
//
// `nimble_port_init()` already calls both `esp_bt_controller_init()` (using
// the chip-specific `BT_CONTROLLER_INIT_CONFIG_DEFAULT()` macro) and
// `esp_bt_controller_enable(ESP_BT_MODE_BLE)` for us. Calling them again
// from Rust returns `ESP_ERR_INVALID_STATE` ("invalid controller state")
// because the controller is already initialised. So we just call
// `nimble_port_init()` and configure the host callbacks afterwards.

unsafe fn bt_stack_init() -> anyhow::Result<()> {
    // 1. Bring up controller + NimBLE host port in one shot.
    esp!(nimble_port_init())?;

    // 2. Wire host callbacks. `sync_cb` fires once when the controller and
    //    host have agreed on initial state — that's where we kick off scan.
    let cfg_ptr = core::ptr::addr_of_mut!(ble_hs_cfg);
    (*cfg_ptr).sync_cb = Some(on_sync);
    // Lowest IO capabilities → "Just Works" pairing, which is what BLE HID
    // gamepads use by default.
    (*cfg_ptr).sm_io_cap = 3 /* BLE_HS_IO_NO_INPUT_OUTPUT */;

    // 3. Standard NimBLE service init (GAP + GATT default services).
    ble_svc_gap_init();
    ble_svc_gatt_init();

    Ok(())
}

// ===========================================================================
// NimBLE event handlers
// ===========================================================================

/// Called by NimBLE when the controller is synchronised with the host.
/// Sets our address type and starts the discovery procedure.
unsafe extern "C" fn on_sync() {
    // Auto-select address type from our identity addresses.
    let mut own_addr_type: u8 = 0;
    let rc = ble_hs_id_infer_auto(0, &mut own_addr_type);
    if rc != 0 {
        warn!("gamepad: ble_hs_id_infer_auto rc={}", rc);
        return;
    }
    OWN_ADDR_TYPE = own_addr_type;

    info!("gamepad: NimBLE synced, starting scan");
    let _ = start_scan();
}

/// Begin (or restart) a general discovery procedure. Returns true on success.
unsafe fn start_scan() -> bool {
    // Cancel any in-flight scan first so a UI "Scan again" press is idempotent.
    let _ = ble_gap_disc_cancel();

    let params = ble_gap_disc_params {
        itvl: 0,           // 0 = stack default
        window: 0,         // 0 = stack default
        filter_policy: 0,  // accept any advertiser
        ..Default::default()
    };

    let rc = ble_gap_disc(
        OWN_ADDR_TYPE,
        BLE_HS_FOREVER,
        &params,
        Some(gap_event_scan),
        core::ptr::null_mut(),
    );
    if rc != 0 {
        warn!("gamepad: ble_gap_disc rc={}", rc);
        false
    } else {
        true
    }
}

/// GAP event handler used during scanning. On a matching advertisement we
/// cancel the scan and switch to the connect handler.
unsafe extern "C" fn gap_event_scan(
    event: *mut ble_gap_event,
    _arg: *mut core::ffi::c_void,
) -> core::ffi::c_int {
    if event.is_null() {
        return 0;
    }
    let ev = &*event;
    if ev.type_ as u32 != BLE_GAP_EVENT_DISC {
        return 0;
    }

    let disc = ev.__bindgen_anon_1.disc;
    if disc.data.is_null() || disc.length_data == 0 {
        return 0;
    }

    // Parse the advertisement payload into the structured form NimBLE provides.
    let mut fields: ble_hs_adv_fields = core::mem::zeroed();
    let rc = ble_hs_adv_parse_fields(&mut fields, disc.data, disc.length_data);
    if rc != 0 {
        return 0;
    }

    // Walk the 16-bit-UUID list looking for the HID Service.
    let mut has_hid = false;
    if !fields.uuids16.is_null() && fields.num_uuids16 > 0 {
        let uuids = core::slice::from_raw_parts(fields.uuids16, fields.num_uuids16 as usize);
        for u in uuids {
            if u.value == HID_SERVICE_UUID16 {
                has_hid = true;
                break;
            }
        }
    }
    if !has_hid {
        return 0;
    }

    // Optional name filter — extract a printable-ish name for logging only.
    let name = if !fields.name.is_null() && fields.name_len > 0 {
        let bytes = core::slice::from_raw_parts(fields.name, fields.name_len as usize);
        String::from_utf8_lossy(bytes).into_owned()
    } else {
        String::from("<no name>")
    };
    info!(
        "gamepad: HID device found '{}' RSSI={} — connecting",
        name, disc.rssi
    );

    // Stop scanning and initiate a connection. The connect callback
    // takes over event reporting on success.
    let _ = ble_gap_disc_cancel();

    let rc = ble_gap_connect(
        OWN_ADDR_TYPE,
        &disc.addr,
        30_000, // 30 s connect timeout
        core::ptr::null(),
        Some(gap_event_conn),
        core::ptr::null_mut(),
    );
    if rc != 0 {
        warn!("gamepad: ble_gap_connect rc={} — restarting scan", rc);
        let _ = start_scan();
    }
    0
}

/// GAP event handler for the active connection. Handles CONNECT,
/// DISCONNECT, and NOTIFY_RX (HID INPUT reports).
unsafe extern "C" fn gap_event_conn(
    event: *mut ble_gap_event,
    _arg: *mut core::ffi::c_void,
) -> core::ffi::c_int {
    if event.is_null() {
        return 0;
    }
    let ev = &*event;

    match ev.type_ as u32 {
        BLE_GAP_EVENT_CONNECT => {
            let c = ev.__bindgen_anon_1.connect;
            if c.status == 0 {
                info!("gamepad: connected, conn_handle={}", c.conn_handle);
                conn_handle_set(c.conn_handle);
                if let Some(state) = global_state() {
                    state.lock().unwrap().connected = true;
                }
                // Discover all characteristics on the link. We then look at
                // each one in `chr_disc_cb` and subscribe to the relevant
                // notifications.
                let rc = ble_gattc_disc_all_chrs(
                    c.conn_handle,
                    1,
                    0xFFFF,
                    Some(chr_disc_cb),
                    core::ptr::null_mut(),
                );
                if rc != 0 {
                    warn!("gamepad: ble_gattc_disc_all_chrs rc={}", rc);
                }
            } else {
                warn!("gamepad: connect failed status={}", c.status);
                let _ = start_scan();
            }
        }
        BLE_GAP_EVENT_DISCONNECT => {
            let d = ev.__bindgen_anon_1.disconnect;
            info!("gamepad: disconnected reason={}", d.reason);
            conn_handle_set(0xFFFF);
            if let Some(state) = global_state() {
                let mut s = state.lock().unwrap();
                s.connected = false;
                s.buttons = [0.0; NUM_BUTTONS];
                s.axes = [0.0; NUM_AXES];
            }
            // Auto-restart scan so the controller can be re-paired by just
            // turning it back on.
            let _ = start_scan();
        }
        BLE_GAP_EVENT_NOTIFY_RX => {
            let n = ev.__bindgen_anon_1.notify_rx;
            // Copy mbuf data into a stack buffer so we don't hold the host
            // task lock while parsing.
            let mut buf = [0u8; 64];
            let mbuf_len = r_os_mbuf_len(n.om) as usize;
            let len = mbuf_len.min(buf.len());
            r_os_mbuf_copydata(
                n.om,
                0,
                len as core::ffi::c_int,
                buf.as_mut_ptr() as *mut core::ffi::c_void,
            );
            if let Some(state) = global_state() {
                let mut s = state.lock().unwrap();
                decode_report(&buf[..len], &mut s);
                s.timestamp = uptime_seconds();
            }
        }
        _ => {}
    }
    0
}

/// Discovery callback for `ble_gattc_disc_all_chrs`. For every characteristic
/// with the NOTIFY property we write 0x0001 to the descriptor that
/// immediately follows its value handle (the CCCD). On well-behaved HID
/// devices (incl. the 8BitDo Ultimate 2C in BLE/Switch mode) this is the
/// Client Characteristic Configuration Descriptor.
///
/// Some devices have multiple notify-capable characteristics (HID Report,
/// Battery Level, etc.). We subscribe to all of them — the report
/// decoder just ignores anything that isn't a HID INPUT report (length < 8).
unsafe extern "C" fn chr_disc_cb(
    conn_handle: u16,
    error: *const ble_gatt_error,
    chr: *const ble_gatt_chr,
    _arg: *mut core::ffi::c_void,
) -> core::ffi::c_int {
    // Discovery completes with `error.status == BLE_HS_EDONE`.
    if !error.is_null() && (*error).status as u32 == BLE_HS_EDONE {
        info!("gamepad: characteristic discovery complete");
        return 0;
    }
    if chr.is_null() {
        return 0;
    }
    let c = &*chr;

    // BLE_GATT_CHR_PROP_NOTIFY = 0x10
    if c.properties & 0x10 == 0 {
        return 0;
    }

    // Optional: log the UUID so we know what we're subscribing to.
    let uuid_kind = c.uuid.u.type_;
    let uuid_val: u32 = match uuid_kind {
        16 /* BLE_UUID_TYPE_16 */ => c.uuid.u16_.value as u32,
        32 /* BLE_UUID_TYPE_32 */ => c.uuid.u32_.value,
        _ => 0,
    };
    info!(
        "gamepad: subscribing to char val_handle=0x{:04x} uuid16=0x{:04x}",
        c.val_handle, uuid_val
    );

    // CCCD is conventionally at val_handle + 1. This is true for the HID
    // Service spec and the 8BitDo controller; for devices that arrange
    // descriptors differently a real descriptor-discovery pass would be
    // required, but adding that round-trip would more than double this
    // file's complexity for no benefit on the target hardware.
    let cccd_handle = c.val_handle + 1;
    let rc = ble_gattc_write_flat(
        conn_handle,
        cccd_handle,
        CCCD_NOTIFY.as_ptr() as *const core::ffi::c_void,
        CCCD_NOTIFY.len() as u16,
        None,
        core::ptr::null_mut(),
    );
    // Best-effort — if this particular char's CCCD lives elsewhere the write
    // returns a non-zero code, which we just log and ignore.
    if rc != 0 && uuid_val == HID_REPORT_UUID16 as u32 {
        warn!("gamepad: CCCD write for HID Report rc={}", rc);
    }
    0
}

// ===========================================================================
// HID report decoder
// ===========================================================================
//
// 8BitDo Ultimate 2C in BLE / "Switch" mode emits a 9–11-byte INPUT report:
//
//   byte 0       : report ID (often 0x01) — present only if the descriptor
//                  declares any report IDs; we sniff it heuristically below
//   bytes 1..=2  : LX (16-bit unsigned, 0..65535, centre 32768)
//   bytes 3..=4  : LY
//   bytes 5..=6  : RX
//   bytes 7..=8  : RY
//   byte 9       : hat (D-pad)  0=N, 1=NE, ..., 7=NW, 15=neutral
//   bytes 10..   : button bitfield (LSB = button 0)
//
// If your unit reports a different layout, log `report` here and tweak the
// offsets — the rest of the pipeline (notification → state → JSON → UI) is
// independent.

fn decode_report(report: &[u8], state: &mut GamepadState) {
    fn axis_u16(lo: u8, hi: u8) -> f32 {
        let raw = u16::from_le_bytes([lo, hi]) as i32;
        let centred = (raw - 0x8000) as f32 / 32768.0;
        if centred.abs() < 0.03 {
            0.0
        } else {
            centred.clamp(-1.0, 1.0)
        }
    }

    state.buttons = [0.0; NUM_BUTTONS];
    state.axes = [0.0; NUM_AXES];

    if report.len() < 8 {
        // Not a HID INPUT (likely battery level or similar) — ignore.
        return;
    }

    // Heuristic: if the first byte is small (1..16) and the rest is ≥ 8
    // bytes, treat byte 0 as a report ID and skip it.
    let body = if report[0] > 0 && report[0] < 16 && report.len() >= 11 {
        &report[1..]
    } else {
        report
    };

    if body.len() >= 8 {
        state.axes[0] = axis_u16(body[0], body[1]);
        state.axes[1] = axis_u16(body[2], body[3]);
        state.axes[2] = axis_u16(body[4], body[5]);
        state.axes[3] = axis_u16(body[6], body[7]);
    }

    // Hat (D-pad) → buttons 12..=15 (up/down/left/right)
    if body.len() >= 9 {
        let hat = body[8] & 0x0F;
        let (up, right, down, left) = match hat {
            0 => (true, false, false, false),
            1 => (true, true, false, false),
            2 => (false, true, false, false),
            3 => (false, true, true, false),
            4 => (false, false, true, false),
            5 => (false, false, true, true),
            6 => (false, false, false, true),
            7 => (true, false, false, true),
            _ => (false, false, false, false),
        };
        state.buttons[12] = if up { 1.0 } else { 0.0 };
        state.buttons[13] = if down { 1.0 } else { 0.0 };
        state.buttons[14] = if left { 1.0 } else { 0.0 };
        state.buttons[15] = if right { 1.0 } else { 0.0 };
    }

    // Remaining bytes are the button bitfield (LSB first).
    if body.len() >= 10 {
        let bits_start = 9;
        let mut bit_index = 0usize;
        for &byte in &body[bits_start..] {
            for b in 0..8 {
                let idx = bit_index;
                bit_index += 1;
                // Hat already occupies 12..=15; only fill the remaining slots.
                if (12..=15).contains(&idx) {
                    continue;
                }
                if idx >= NUM_BUTTONS {
                    break;
                }
                if byte & (1 << b) != 0 {
                    state.buttons[idx] = 1.0;
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Utilities
// ---------------------------------------------------------------------------

fn uptime_seconds() -> f64 {
    let us = unsafe { esp_timer_get_time() };
    us as f64 / 1_000_000.0
}
