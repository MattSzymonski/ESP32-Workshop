// This file implements the BLE gamepad module for the HTTP server.
// - Connects to a BLE HID gamepad (e.g. 8BitDo Ultimate 2C in BT mode) using the
//   esp32-nimble crate as a BLE central.
// - Discovers the standard HID Service (UUID 0x1812), reads the Report Map
//   characteristic (0x2A4B) once for diagnostics, and subscribes to every
//   Report characteristic (0x2A4D) marked with the Notify property.
// - Stores the latest raw input report bytes and a millisecond timestamp in a
//   shared `Arc<Mutex<GamepadState>>` written from the BLE notification callback.
// - Registers `GET /api/gamepad` returning a JSON snapshot:
//     {"connected":bool,"name":str,"addr":str,"ts":u64,"report":hex,"reportMap":hex}
//   The browser-side card decodes `report` into 4 axes + 18 button bits, mirroring
//   the layout used by classic Web Gamepad API testers.
// - The BLE state machine runs on its own dedicated thread (BLEDevice/BLEClient
//   are !Send), reconnecting automatically on disconnect or scan timeout.
// - Depends on: esp32-nimble (BLE central), esp-idf-svc (HTTP server),
//   bstr (re-exported by esp32-nimble for advert name matching).

use bstr::ByteSlice;
use embedded_io::Write;
use esp32_nimble::{
    enums::{AuthReq, SecurityIOCap},
    utilities::BleUuid,
    BLEDevice, BLEScan,
};
use esp_idf_svc::hal::delay::FreeRtos;
use esp_idf_svc::hal::task::block_on;
use esp_idf_svc::http::server::EspHttpServer;
use log::{info, warn};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const CARD_HTML: &str = include_str!("card.html");

// Standard BLE HID GATT UUIDs (Bluetooth SIG 16-bit assigned numbers)
const HID_SERVICE_UUID: BleUuid = BleUuid::Uuid16(0x1812);
const HID_REPORT_UUID: BleUuid = BleUuid::Uuid16(0x2A4D);
const HID_REPORT_MAP_UUID: BleUuid = BleUuid::Uuid16(0x2A4B);

// How long each scan session lasts before we give up and retry (milliseconds)
const SCAN_TIMEOUT_MS: u32 = 30_000;
// Cooldown between connection attempts after disconnect/error
const RECONNECT_DELAY: Duration = Duration::from_secs(2);

// Snapshot of the gamepad state shared between the BLE thread and HTTP handler.
// Written from the NimBLE notification callback, read from HTTP requests.
#[derive(Default, Clone)]
struct GamepadState {
    connected: bool,
    name: String,
    addr: String,
    timestamp_ms: u64,
    report: Vec<u8>,
    report_map: Vec<u8>,
}

/// Registers the gamepad endpoint on the server and returns:
/// - the HTML card to embed in the main page
/// - a `GamepadHandle` whose `start()` method spawns the BLE state machine.
///
/// Endpoint: `GET /api/gamepad`
/// Returns `{"connected":<bool>,"name":<str>,"addr":<str>,"ts":<ms>,
///           "report":<hex>,"reportMap":<hex>}`.
///
/// BLE init is intentionally deferred: NimBLE's first scan allocates several
/// kilobytes of mbufs/control blocks, and if it runs before the main page
/// string is assembled the heap may be too fragmented to allocate the
/// ~28 KiB contiguous buffer for the rendered HTML page. Call `handle.start()`
/// only after the main page string has been built.
pub fn register(server: &mut EspHttpServer) -> anyhow::Result<(String, GamepadHandle)> {
    // 1. Shared snapshot — written by BLE thread / notify callback, read by HTTP handler.
    let state: Arc<Mutex<GamepadState>> = Arc::new(Mutex::new(GamepadState::default()));

    // Shared one-shot "please vibrate now" flag. Set by the HTTP handler,
    // polled and cleared by the BLE thread. AtomicBool is ideal here — no
    // mutex contention with the much-busier `state` lock.
    let vibrate_request: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));

    // Number of writable HID Report characteristics discovered on the last
    // successful connection.  Written once per connection by the BLE thread,
    // read by the vibrate HTTP handler so it can tell the browser whether
    // rumble is actually available on the connected device.
    let writable_chars: Arc<std::sync::atomic::AtomicU8> =
        Arc::new(std::sync::atomic::AtomicU8::new(0));

    // 2. Register GET /api/gamepad immediately so the handler exists even
    //    before the BLE radio comes up. Until `start()` is called, it returns
    //    the default "disconnected" snapshot.
    let state_for_handler = state.clone();
    server.fn_handler("/api/gamepad", esp_idf_svc::http::Method::Get, move |req| {
        let snapshot = state_for_handler.lock().unwrap().clone();
        let json = serialize_snapshot(&snapshot);
        let mut response =
            req.into_response(200, Some("OK"), &[("Content-Type", "application/json")])?;
        response.write(json.as_bytes())?;
        Ok::<(), anyhow::Error>(())
    })?;

    // 2b. Vibration test endpoint. Just sets the flag — actual BLE write happens
    //     on the gamepad-ble thread (BLE characteristics are !Send).
    //     Returns 200 even if no controller is paired; the BLE thread silently
    //     drops the request in that case.
    let vibrate_for_handler = vibrate_request.clone();
    let writable_chars_for_handler = writable_chars.clone();
    server.fn_handler(
        "/api/gamepad/vibrate",
        esp_idf_svc::http::Method::Get,
        move |req| {
            let n = writable_chars_for_handler.load(Ordering::Relaxed);
            vibrate_for_handler.store(true, Ordering::Relaxed);
            let mut response =
                req.into_response(200, Some("OK"), &[("Content-Type", "application/json")])?;
            // Return the writable-char count so the browser can tell whether
            // the controller even supports rumble (n==0 means no output Report).
            let body = format!(r#"{{"ok":true,"chars":{}}}"#, n);
            response.write(body.as_bytes())?;
            Ok::<(), anyhow::Error>(())
        },
    )?;

    info!("gamepad: registered /api/gamepad and /api/gamepad/vibrate (BLE not started yet)");
    Ok((
        CARD_HTML.to_string(),
        GamepadHandle {
            state,
            vibrate_request,
            writable_chars,
        },
    ))
}

/// Opaque handle returned by [`register`]. Hold on to it and call
/// [`GamepadHandle::start`] to spawn the BLE state machine.
pub struct GamepadHandle {
    state: Arc<Mutex<GamepadState>>,
    vibrate_request: Arc<AtomicBool>,
    writable_chars: Arc<std::sync::atomic::AtomicU8>,
}

impl GamepadHandle {
    /// Spawn the dedicated BLE thread. BLEDevice/BLEClient are `!Send`, so
    /// they must live entirely on this thread. 8 KiB is enough because the
    /// heavy NimBLE host work runs on a separate FreeRTOS task sized by
    /// `CONFIG_BT_NIMBLE_HOST_TASK_STACK_SIZE`; this thread only drives the
    /// scan/connect/subscribe state machine and forwards notifications.
    ///
    /// If spawn fails (heap fragmented after Wi-Fi + framebuffers), we log
    /// and continue: the `/api/gamepad` endpoint stays alive and just keeps
    /// reporting `"connected": false`.
    pub fn start(self) {
        let state = self.state;
        let vibrate = self.vibrate_request;
        let writable_chars_counter = self.writable_chars;
        match std::thread::Builder::new()
            .stack_size(8 * 1024)
            .name("gamepad-ble".into())
            .spawn(move || ble_loop(state, vibrate, writable_chars_counter))
        {
            Ok(_) => info!("gamepad: BLE state machine started"),
            Err(e) => warn!(
                "gamepad: failed to spawn BLE task ({:?}); endpoint will report disconnected",
                e
            ),
        }
    }
}

// Serializes the snapshot to the JSON shape expected by the front-end card.
// Hex encoding is used for the binary blobs to keep the JSON parser simple.
fn serialize_snapshot(s: &GamepadState) -> String {
    let report_hex = bytes_to_hex(&s.report);
    let map_hex = bytes_to_hex(&s.report_map);
    let name_escaped = json_escape(&s.name);
    let addr_escaped = json_escape(&s.addr);
    format!(
        r#"{{"connected":{},"name":"{}","addr":"{}","ts":{},"report":"{}","reportMap":"{}"}}"#,
        s.connected, name_escaped, addr_escaped, s.timestamp_ms, report_hex, map_hex
    )
}

fn bytes_to_hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        // Manual hex formatting — avoids pulling in `hex` crate.
        const HEX: &[u8; 16] = b"0123456789abcdef";
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

// Outer reconnection loop. Any error from `connect_and_run` is logged and the
// loop sleeps briefly before scanning again. The shared state is reset to
// "disconnected" between attempts so the UI reflects reality.
fn ble_loop(
    state: Arc<Mutex<GamepadState>>,
    vibrate: Arc<AtomicBool>,
    writable_chars: Arc<std::sync::atomic::AtomicU8>,
) {
    loop {
        if let Err(e) = connect_and_run(&state, &vibrate, &writable_chars) {
            warn!("gamepad: BLE session ended: {:?}", e);
        }
        // Mark disconnected so the UI stops claiming we're paired.
        {
            let mut s = state.lock().unwrap();
            s.connected = false;
        }
        // Drop any stale vibration request—it would be applied to whichever
        // controller pairs next, which is surprising.
        vibrate.store(false, Ordering::Relaxed);
        writable_chars.store(0, Ordering::Relaxed);
        std::thread::sleep(RECONNECT_DELAY);
    }
}

// One full BLE session: scan → connect → discover HID service → subscribe to
// all Report characteristics → poll for vibration requests until disconnect.
fn connect_and_run(
    state: &Arc<Mutex<GamepadState>>,
    vibrate: &Arc<AtomicBool>,
    writable_chars: &Arc<std::sync::atomic::AtomicU8>,
) -> anyhow::Result<()> {
    let writable_chars = writable_chars.clone();
    block_on(async move {
        // 1. Take the singleton BLE stack handle and configure security.
        //    8BitDo controllers require bonding; NoInputNoOutput uses Just-Works
        //    pairing which is sufficient for HID over LE.
        let ble_device = BLEDevice::take();
        ble_device
            .security()
            .set_auth(AuthReq::all())
            .set_io_cap(SecurityIOCap::NoInputNoOutput);

        // 2. Scan for the gamepad. We accept any device whose advertised local
        //    name contains a known substring. The name is captured into a shared
        //    cell so we can present it in the UI after the scan completes.
        let name_cell: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
        let name_cell_for_filter = name_cell.clone();

        info!("gamepad: scanning for BLE HID gamepad...");
        let mut scan = BLEScan::new();
        let device = scan
            .active_scan(true)
            .interval(100)
            .window(99)
            .start(ble_device, SCAN_TIMEOUT_MS as i32, move |dev, data| {
                if let Some(name) = data.name() {
                    if name.contains_str("8BitDo")
                        || name.contains_str("Ultimate")
                        || name.contains_str("Controller")
                        || name.contains_str("Gamepad")
                    {
                        if let Ok(mut g) = name_cell_for_filter.lock() {
                            *g = String::from_utf8_lossy(name.as_bytes()).into_owned();
                        }
                        return Some(*dev);
                    }
                }
                None
            })
            .await?;

        let device =
            device.ok_or_else(|| anyhow::anyhow!("no gamepad found within scan window"))?;
        let name = name_cell.lock().unwrap().clone();
        let addr = format!("{:?}", device.addr());
        info!("gamepad: found '{}' @ {}", name, addr);

        // 3. Connect. The on_disconnect hook flips a shared atomic so the
        //    poll loop below can detect link loss without holding an
        //    immutable borrow on `client` (which conflicts with the mutable
        //    borrow `service` keeps for the lifetime of the writable Report
        //    references we collect later).
        let connected_flag = Arc::new(AtomicBool::new(true));
        let connected_for_cb = connected_flag.clone();
        let mut client = ble_device.new_client();
        client.on_connect(|c| {
            // 7.5–15 ms connection interval, 0 latency, 2 s supervision timeout —
            // typical for low-latency HID input.
            let _ = c.update_conn_params(6, 12, 0, 200);
        });
        client.on_disconnect(move |reason| {
            info!("gamepad: peer disconnected (reason={})", reason);
            connected_for_cb.store(false, Ordering::Relaxed);
        });

        client.connect(&device.addr()).await?;
        info!("gamepad: GATT connection established");

        // Try to bond / encrypt. Some controllers refuse HID notifications until
        // the link is encrypted. Failure here is non-fatal: many controllers also
        // expose unencrypted HID Reports.
        if let Err(e) = client.secure_connection().await {
            warn!("gamepad: secure_connection failed (continuing): {:?}", e);
        }

        // Publish "connected" state to the UI.
        {
            let mut s = state.lock().unwrap();
            s.connected = true;
            s.name = name;
            s.addr = addr;
            s.timestamp_ms = now_ms();
            s.report.clear();
            s.report_map.clear();
        }

        // 4. Discover the HID service (mandatory) and read the Report Map once.
        let service = client.get_service(HID_SERVICE_UUID).await?;

        // Report Map (HID descriptor) — read once for diagnostics. Failure is
        // non-fatal because some stacks gate it behind encryption.
        match service.get_characteristic(HID_REPORT_MAP_UUID).await {
            Ok(rm) => match rm.read_value().await {
                Ok(map) => {
                    info!("gamepad: HID report map ({} bytes)", map.len());
                    state.lock().unwrap().report_map = map;
                }
                Err(e) => warn!("gamepad: failed to read report map: {:?}", e),
            },
            Err(e) => warn!("gamepad: report map characteristic not found: {:?}", e),
        }

        // 5. Walk every Report characteristic. We do TWO things here:
        //    - subscribe to notifying ones (controller → host: input reports)
        //    - remember the writable ones (host → controller: output reports,
        //      typically used for rumble/vibration)
        //
        //    These are almost always *different* characteristics on a
        //    real-world controller (input report 0x01 vs output report 0x02
        //    in the HID descriptor), so we route each char into exactly one
        //    of the two buckets to keep the borrow checker happy: pushing
        //    `&mut c` into `writable_reports` consumes the reference, so we
        //    couldn't subscribe to it afterwards anyway.
        let mut subscribed = 0usize;
        let mut writable_reports: Vec<&mut esp32_nimble::BLERemoteCharacteristic> = Vec::new();
        let chars = service.get_characteristics().await?;
        for c in chars {
            let uuid_match = c.uuid() == HID_REPORT_UUID;
            if !uuid_match {
                continue;
            }
            let is_writable = c.can_write() || c.can_write_no_response();
            if is_writable {
                // Log so we can see what the controller actually exposes.
                // (`handle()` is the GATT attribute handle, useful for matching
                // against `btmon`/`hcitool` traces.)
                info!(
                    "gamepad: writable Report char: write={}, write_no_response={}, notify={}",
                    c.can_write(),
                    c.can_write_no_response(),
                    c.can_notify()
                );
                writable_reports.push(c);
            } else if c.can_notify() {
                let state_for_cb = state.clone();
                c.on_notify(move |data| {
                    if data.is_empty() {
                        return;
                    }
                    let now = now_ms();
                    if let Ok(mut s) = state_for_cb.lock() {
                        // Replace the latest report buffer in-place.
                        s.report.clear();
                        s.report.extend_from_slice(data);
                        s.timestamp_ms = now;
                    }
                })
                .subscribe_notify(false)
                .await?;
                subscribed += 1;
            }
        }
        info!(
            "gamepad: subscribed to {} HID Report notification(s), found {} writable Report char(s) for rumble",
            subscribed,
            writable_reports.len()
        );
        writable_chars.store(writable_reports.len() as u8, Ordering::Relaxed);
        if subscribed == 0 {
            return Err(anyhow::anyhow!(
                "no notifying HID Report characteristics on this device"
            ));
        }

        // 6. Idle until the link drops, polling for vibration requests.
        //    Notification delivery is callback-driven (see above) so this loop
        //    only services rumble. 100 ms polling = barely measurable extra
        //    CPU and gives a snappy "press button → buzz" feel.
        //    We use the `connected_flag` set by `on_disconnect` instead of
        //    `client.connected()` because the latter would require borrowing
        //    `client` immutably, which conflicts with the mutable borrow that
        //    `service` (and its `writable_reports`) holds.
        const POLL_MS: u32 = 100;
        while connected_flag.load(Ordering::Relaxed) {
            if vibrate.swap(false, Ordering::Relaxed) {
                if let Err(e) = send_vibration(&mut writable_reports).await {
                    warn!("gamepad: vibration write failed: {:?}", e);
                } else {
                    info!("gamepad: vibration triggered");
                }
            }
            FreeRtos::delay_ms(POLL_MS);
        }
        info!("gamepad: connection lost, returning to scan loop");
        Ok::<(), anyhow::Error>(())
    })
}

// Sends a one-shot rumble command to every writable HID Report characteristic.
//
// HID rumble over BLE is NOT standardised.  We try every known format for the
// 8BitDo Ultimate 2C in B-mode (and common alternatives), logging each
// attempt so the serial monitor shows exactly which characteristic + payload
// combination the controller accepted.
//
// Formats tried (in order, per writable characteristic):
//   1. 8BitDo / generic Android FF -- [0x03, intensity]
//      8BitDo controllers in B-mode expose a 2-byte output report where
//      byte 0 = report type (0x03 = rumble) and byte 1 = intensity (0..0xFF).
//   2. Bare strong+weak pair -- [0xFF, 0xFF]
//      Many cheap clones and Sony DualSense variants.
//   3. Plain single-byte intensity -- [0xFF]
//   4. Xbox One BLE rumble -- 8 bytes [0x0F, 0, 0, 80, 80, 100, 0, 0]
//      Microsoft reference format; some firmwares also speak this.
async fn send_vibration(
    writables: &mut [&mut esp32_nimble::BLERemoteCharacteristic],
) -> anyhow::Result<()> {
    if writables.is_empty() {
        warn!("gamepad: no writable HID Report char - controller does not expose rumble");
        return Ok(());
    }

    const PAYLOADS: &[&[u8]] = &[
        &[0xFF, 0xFF],                    // plain strong+weak (most common Android HID)
        &[0x00, 0xFF, 0xFF, 0x00],        // Android 4-byte FF: duration, strong, weak, extra
        &[0x03, 0xFF],                    // 8BitDo type=0x03 rumble
        &[0x0F, 0, 0, 80, 80, 100, 0, 0], // Xbox One BLE rumble
    ];

    let mut last_err: Option<anyhow::Error> = None;
    for (ci, c) in writables.iter_mut().enumerate() {
        // Try with-response first (more reliable), then without-response.
        // Some controllers only accept one mode.
        for (pi, payload) in PAYLOADS.iter().enumerate() {
            for &with_response in &[true, false] {
                if with_response && !c.can_write() {
                    continue;
                }
                if !with_response && !c.can_write_no_response() {
                    continue;
                }
                match c.write_value(payload, with_response).await {
                    Ok(()) => {
                        info!(
                            "gamepad: rumble OK - char#{} payload#{} ({} bytes, response={})",
                            ci,
                            pi,
                            payload.len(),
                            with_response
                        );
                        return Ok(());
                    }
                    Err(e) => {
                        warn!(
                            "gamepad: rumble try failed - char#{} payload#{} ({} bytes, response={}): {:?}",
                            ci, pi, payload.len(), with_response, e
                        );
                        last_err = Some(anyhow::anyhow!("{:?}", e));
                    }
                }
            }
        }
    }
    Err(last_err
        .unwrap_or_else(|| anyhow::anyhow!("no writable Report accepted any rumble payload")))
}

// Wall-clock-ish millisecond timer using ESP-IDF's monotonic 64-bit µs clock.
// Used for both the front-end "TIMESTAMP" field and to time-stamp reports.
fn now_ms() -> u64 {
    (unsafe { esp_idf_svc::sys::esp_timer_get_time() } as u64) / 1000
}
