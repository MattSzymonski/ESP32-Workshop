// This file implements the WS2812 RGB LED module for the HTTP server.
// - Drives a single WS2812B addressable LED on GPIO8 via the ESP32 RMT peripheral.
// - Registers GET /api/blink: turns the LED white for 1 second then switches it off.
// - The LED driver is wrapped in Arc<Mutex<>> to allow safe sharing with the request closure.
// - Depends on: ws2812-esp32-rmt-driver, smart-leds, esp-idf-svc (RMT, GPIO, HTTP server).

use embedded_io::Write;
use esp_idf_svc::hal::gpio::OutputPin;
use esp_idf_svc::hal::rmt::RmtChannel;
use esp_idf_svc::http::server::EspHttpServer;
use smart_leds::{SmartLedsWrite, RGB8};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use ws2812_esp32_rmt_driver::Ws2812Esp32Rmt;

const CARD_HTML: &str = include_str!("card.html");

/// Registers the WS2812 LED blink endpoint on the server and returns the HTML card
/// to be embedded in the main page's center column.
///
/// Endpoint: `GET /api/blink`
/// Turns the LED white for 1 second then off. Responds with `{"blinked":true}`.
pub fn register(
    server: &mut EspHttpServer,
    rmt_channel: impl RmtChannel + Send + 'static,
    pin: impl OutputPin + Send + 'static,
) -> anyhow::Result<String> {
    // 1. Initialise the WS2812 RMT driver and wrap in Arc<Mutex<>> for closure sharing
    let led = Ws2812Esp32Rmt::new(rmt_channel, pin)?;
    let led = Arc::new(Mutex::new(led));

    // 2. Register the HTTP endpoint
    server.fn_handler("/api/blink", esp_idf_svc::http::Method::Get, move |req| {
        {
            // Acquire the mutex guard; dropped at the end of this inner block
            let mut led = led.lock().unwrap();

            // 2a. Turn the LED on (dim white)
            led.write(
                [RGB8 {
                    r: 50,
                    g: 50,
                    b: 50,
                }]
                .into_iter(),
            )?;
            // 2b. Hold for one second
            std::thread::sleep(Duration::from_secs(1));
            // 2c. Turn the LED off
            led.write([RGB8 { r: 0, g: 0, b: 0 }].into_iter())?;
        } // LED mutex released here

        // 2d. Send JSON confirmation response
        let headers = [("Content-Type", "application/json")];
        let mut response = req.into_response(200, Some("OK"), &headers)?;
        response.write(br#"{"blinked":true}"#)?;
        Ok::<(), anyhow::Error>(())
    })?;

    // 3. Return the compile-time-embedded HTML card for this module
    Ok(CARD_HTML.to_string())
}
