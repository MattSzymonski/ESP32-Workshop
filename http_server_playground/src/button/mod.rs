// This file implements the button counter module for the HTTP server.
// - Monitors the button on GPIO9 (onboard BOOT button on ESP32-C6-DevKitC-1) as a digital input.
// - A background thread polls the pin every 50 ms and detects falling edges (active-low button).
// - Registers GET /api/button: returns the current press count.
// - Registers GET /api/button/reset: resets the count to zero and returns the new value.
// - The press counter is an AtomicU32 shared between the poll thread and both HTTP handlers.
// - Depends on: esp-idf-svc (GPIO, HTTP server).

use embedded_io::Write;
use esp_idf_svc::hal::gpio::{Gpio9, Input, PinDriver, Pull};
use esp_idf_svc::http::server::EspHttpServer;
use log::info;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

const CARD_HTML: &str = include_str!("card.html");

// Polling interval in milliseconds. Acts as implicit debounce:
// a press shorter than this interval may be missed, but bounces shorter
// than this interval are also ignored.
const POLL_INTERVAL_MS: u64 = 50;

/// Registers the button counter endpoints on the server and returns the HTML card
/// to be embedded in the main page's center column.
///
/// Endpoint: `GET /api/button`       — `{"count": N}`
/// Endpoint: `GET /api/button/reset` — resets count, returns `{"count": 0}`
pub fn register(server: &mut EspHttpServer, pin: Gpio9) -> anyhow::Result<String> {
    // 1. Transmute the peripheral lifetime to 'static so the pin can be moved
    //    into the background thread. Safe because ESP32 peripherals live for the
    //    entire program lifetime.
    let pin: Gpio9<'static> = unsafe { core::mem::transmute(pin) };

    // 2. Shared press counter — written by the poll thread, read by both HTTP handlers.
    let press_count = Arc::new(AtomicU32::new(0));
    let press_count_for_thread = press_count.clone();

    // 3. Spawn a background thread that polls the pin and counts falling edges.
    //    GPIO9 / BOOT button is active-low: the pin is HIGH at rest and goes LOW when pressed.
    std::thread::Builder::new()
        .stack_size(2048)
        .spawn(move || {
            let mut driver = PinDriver::input(pin, Pull::Up).expect("button: failed to init GPIO9");

            let mut previous_high = true; // pin starts high (not pressed)

            loop {
                let currently_high = driver.is_high();

                // Falling edge: pin was HIGH last sample, now LOW → button pressed
                if previous_high && !currently_high {
                    press_count_for_thread.fetch_add(1, Ordering::Relaxed);
                }

                previous_high = currently_high;
                std::thread::sleep(Duration::from_millis(POLL_INTERVAL_MS));
            }
        })
        .expect("button: failed to spawn poll thread");

    // 4. Register GET /api/button — returns the current count
    let press_count_for_read = press_count.clone();
    server.fn_handler("/api/button", esp_idf_svc::http::Method::Get, move |req| {
        let count = press_count_for_read.load(Ordering::Relaxed);
        let mut response =
            req.into_response(200, Some("OK"), &[("Content-Type", "application/json")])?;
        response.write(format!(r#"{{"count":{}}}"#, count).as_bytes())?;
        Ok::<(), anyhow::Error>(())
    })?;

    // 5. Register GET /api/button/reset — resets the count to zero and returns it
    let press_count_for_reset = press_count.clone();
    server.fn_handler(
        "/api/button/reset",
        esp_idf_svc::http::Method::Get,
        move |req| {
            press_count_for_reset.store(0, Ordering::Relaxed);
            let mut response =
                req.into_response(200, Some("OK"), &[("Content-Type", "application/json")])?;
            response.write(br#"{"count":0}"#)?;
            Ok::<(), anyhow::Error>(())
        },
    )?;

    // 6. Return the compile-time-embedded HTML card for this module
    info!("button: registered /api/button and /api/button/reset");
    Ok(CARD_HTML.to_string())
}
