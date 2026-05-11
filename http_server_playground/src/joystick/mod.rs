// This file implements the joystick module for the HTTP server.
// - Reads VRX on GPIO0 (ADC1 CH0) and VRY on GPIO3 (ADC1 CH3) as 12-bit raw counts.
// - Monitors the SW press button on GPIO20 (active-low, internal pull-up).
// - A background thread polls SW every 50 ms and records the timestamp of each falling edge.
// - Registers GET /api/joystick: returns {"x":<0-4095>,"y":<0-4095>,"sw":<true|false>}
//   where "sw" is true for 200 ms after the most recent button press.
// - Accepts a shared Arc<AdcDriver<ADCU1>> so ADC1 hardware is shared with other modules.
// - Depends on: esp-idf-svc (ADC oneshot, GPIO, HTTP server).

use esp_idf_svc::hal::adc::attenuation::DB_12;
use esp_idf_svc::hal::adc::oneshot::config::AdcChannelConfig;
use esp_idf_svc::hal::adc::oneshot::{AdcChannelDriver, AdcDriver};
use esp_idf_svc::hal::adc::ADCU1;
use esp_idf_svc::hal::gpio::{Gpio0, Gpio20, Gpio3, PinDriver, Pull};
use esp_idf_svc::http::server::EspHttpServer;
use log::info;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const CARD_HTML: &str = include_str!("card.html");

// SW button stays "highlighted" in the UI for this many milliseconds after a press
const SW_HIGHLIGHT_MS: u64 = 200;

// Polling interval for the SW background thread (also acts as debounce window)
const SW_POLL_INTERVAL_MS: u64 = 50;

/// Registers the joystick endpoint on the server and returns the HTML card.
///
/// Endpoint: `GET /api/joystick`
/// Returns `{"x":<0-4095>,"y":<0-4095>,"sw":<true|false>}`.
/// `sw` is true for 200 ms after the most recent button press.
pub fn register(
    server: &mut EspHttpServer,
    adc_driver: Arc<AdcDriver<'static, ADCU1>>,
    vrx_pin: Gpio0,
    vry_pin: Gpio3,
    sw_pin: Gpio20,
) -> anyhow::Result<String> {
    // 1. Transmute pin lifetimes to 'static so they can be moved into the
    //    handler closure / background thread. Safe: ESP32 peripherals live
    //    for the entire program lifetime.
    let vrx_pin: Gpio0<'static> = unsafe { core::mem::transmute(vrx_pin) };
    let vry_pin: Gpio3<'static> = unsafe { core::mem::transmute(vry_pin) };
    let sw_pin: Gpio20<'static> = unsafe { core::mem::transmute(sw_pin) };

    // 2. Configure both ADC channels at DB_12 attenuation (0–3.3 V range).
    //    Arc<AdcDriver> is cloned into each channel so the driver stays alive.
    let channel_config = AdcChannelConfig {
        attenuation: DB_12,
        ..Default::default()
    };
    let vrx_channel = AdcChannelDriver::new(adc_driver.clone(), vrx_pin, &channel_config)?;
    let vry_channel = AdcChannelDriver::new(adc_driver.clone(), vry_pin, &channel_config)?;

    // 3. Wrap both channels in a single Mutex so the HTTP handler reads them
    //    atomically and they are never accessed concurrently.
    let channels = Arc::new(Mutex::new((vrx_channel, vry_channel)));

    // 4. AtomicU32 storing the ESP timer timestamp (ms) of the last SW press.
    //    u32 wraps after ~49 days of uptime which is acceptable.
    //    The handler computes elapsed time to decide whether to show the highlight.
    let last_press_ms: Arc<AtomicU32> = Arc::new(AtomicU32::new(0));
    let last_press_for_thread = last_press_ms.clone();

    // 5. Background thread: poll SW pin every 50 ms, detect falling edge (active-low).
    std::thread::Builder::new()
        .stack_size(2048)
        .spawn(move || {
            let mut sw =
                PinDriver::input(sw_pin, Pull::Up).expect("joystick: failed to init GPIO20 (SW)");
            let mut previous_high = true;
            loop {
                let currently_high = sw.is_high();
                if previous_high && !currently_high {
                    // Falling edge — record current µs timestamp
                    let now_ms =
                        (unsafe { esp_idf_svc::sys::esp_timer_get_time() as u64 } / 1000) as u32;
                    last_press_for_thread.store(now_ms, Ordering::Relaxed);
                }
                previous_high = currently_high;
                std::thread::sleep(Duration::from_millis(SW_POLL_INTERVAL_MS));
            }
        })
        .expect("joystick: failed to spawn SW poll thread");

    // 6. Register the HTTP endpoint
    server.fn_handler(
        "/api/joystick",
        esp_idf_svc::http::Method::Get,
        move |req| {
            // 6a. Read both ADC channels (raw 12-bit counts, 0 = min, 4095 = max)
            let (x, y) = {
                let mut guard = channels.lock().unwrap();
                let (vrx, vry) = &mut *guard;
                let x = vrx.read_raw()?;
                let y = vry.read_raw()?;
                (x, y)
            };

            // 6b. Check whether the SW highlight window is still active
            let now_ms = (unsafe { esp_idf_svc::sys::esp_timer_get_time() as u64 } / 1000) as u32;
            let last_ms = last_press_ms.load(Ordering::Relaxed);
            let sw_active = last_ms > 0 && now_ms.saturating_sub(last_ms) < SW_HIGHLIGHT_MS as u32;

            // 6c. Send JSON response
            let mut response =
                req.into_response(200, Some("OK"), &[("Content-Type", "application/json")])?;
            let json = format!(
                r#"{{"x":{},"y":{},"sw":{}}}"#,
                x,
                y,
                if sw_active { "true" } else { "false" }
            );
            response.write(json.as_bytes())?;
            Ok::<(), anyhow::Error>(())
        },
    )?;

    // 7. Return the compile-time-embedded HTML card for this module
    info!("joystick: registered /api/joystick");
    Ok(CARD_HTML.to_string())
}
