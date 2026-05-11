// This file implements the buzzer module for the HTTP server.
// - Drives a passive or active buzzer on GPIO15 via the ESP32 LEDC PWM peripheral.
// - Registers GET /api/beep?duration_ms=<1-5000>&frequency_hz=<100-8000>: sounds the buzzer.
// - Duration defaults to 200 ms (clamped 1–5000 ms). Frequency defaults to 2000 Hz (clamped 100–8000 Hz).
// - Pitch is set by calling LedcTimerDriver::set_frequency before enabling the duty cycle.
// - The LEDC timer and channel drivers are kept alive in Arc<Mutex<>> for the server lifetime.
// - Depends on: esp-idf-svc (LEDC, GPIO, HTTP server).

use embedded_io::Write;
use esp_idf_svc::hal::gpio::OutputPin;
use esp_idf_svc::hal::ledc::{
    config::TimerConfig, LedcChannel, LedcDriver, LedcTimer, LedcTimerDriver, Resolution,
};
use esp_idf_svc::hal::units::Hertz;
use esp_idf_svc::http::server::EspHttpServer;
use log::info;
use std::sync::{Arc, Mutex};
use std::time::Duration;

const CARD_HTML: &str = include_str!("card.html");

// 50 % duty cycle produces the loudest square wave on a passive buzzer.
const DUTY_ON: u32 = 128; // half of 8-bit full-scale (255)
const DUTY_OFF: u32 = 0;

const DEFAULT_DURATION_MS: u64 = 200;
const MAX_DURATION_MS: u64 = 5000;

const DEFAULT_FREQUENCY_HZ: u32 = 2000;
const MIN_FREQUENCY_HZ: u32 = 100;
const MAX_FREQUENCY_HZ: u32 = 8000;

/// Registers the beeper endpoint on the server and returns the HTML card
/// to be embedded in the main page's center column.
///
/// Endpoint: `GET /api/beep?duration_ms=<1-5000>&frequency_hz=<100-8000>`
/// Sounds the buzzer at the requested pitch for the requested number of milliseconds.
pub fn register<T, C>(
    server: &mut EspHttpServer,
    timer: T,
    channel: C,
    pin: impl OutputPin + 'static,
) -> anyhow::Result<String>
where
    T: LedcTimer + 'static,
    C: LedcChannel<SpeedMode = T::SpeedMode> + 'static,
    LedcTimerDriver<'static, T::SpeedMode>: Send,
{
    // 1. Configure the LEDC timer: 2 kHz, 8-bit resolution.
    //    8-bit gives 255 ticks per period; 50 % duty = 128 ticks.
    let timer_driver = LedcTimerDriver::new(
        timer,
        &TimerConfig::new()
            .frequency(Hertz(DEFAULT_FREQUENCY_HZ))
            .resolution(Resolution::Bits8),
    )?;

    // 2. Create the LEDC PWM channel bound to the buzzer signal pin.
    //    Both drivers are bundled into a single Arc<Mutex<>> so the timer
    //    driver is kept alive (dropping it would stop the PWM output).
    //    The channel starts with duty = 0 so the buzzer is silent at boot.
    let ledc_driver = LedcDriver::new(channel, &timer_driver, pin)?;
    let buzzer = Arc::new(Mutex::new((timer_driver, ledc_driver)));

    // 3. Register the HTTP endpoint
    server.fn_handler("/api/beep", esp_idf_svc::http::Method::Get, move |req| {
        // 3a. Parse ?duration_ms=N&frequency_hz=N from the request URI
        let query = req.uri().split_once('?').map(|(_, q)| q).unwrap_or("");
        let duration_ms: u64 = query
            .split('&')
            .find(|part| part.starts_with("duration_ms="))
            .and_then(|part| part["duration_ms=".len()..].parse::<u64>().ok())
            .unwrap_or(DEFAULT_DURATION_MS)
            .clamp(1, MAX_DURATION_MS);
        let frequency_hz: u32 = query
            .split('&')
            .find(|part| part.starts_with("frequency_hz="))
            .and_then(|part| part["frequency_hz=".len()..].parse::<u32>().ok())
            .unwrap_or(DEFAULT_FREQUENCY_HZ)
            .clamp(MIN_FREQUENCY_HZ, MAX_FREQUENCY_HZ);

        // 3b. Sound the buzzer: update frequency, set 50 % duty, sleep, then silence
        {
            let mut guard = buzzer.lock().unwrap();
            let (timer_driver, ledc_driver) = &mut *guard;
            // Changing frequency reconfigures the hardware timer; duty is preserved.
            timer_driver.set_frequency(Hertz(frequency_hz))?;
            ledc_driver.set_duty(DUTY_ON)?;
            drop(guard); // Release the lock while sleeping so other requests are not blocked
            std::thread::sleep(Duration::from_millis(duration_ms));
            let mut guard = buzzer.lock().unwrap();
            let (_, ledc_driver) = &mut *guard;
            ledc_driver.set_duty(DUTY_OFF)?;
        }

        // 3c. Send JSON confirmation response
        let mut response =
            req.into_response(200, Some("OK"), &[("Content-Type", "application/json")])?;
        let json = format!(
            r#"{{"beeped":true,"duration_ms":{},"frequency_hz":{}}}"#,
            duration_ms, frequency_hz
        );
        response.write(json.as_bytes())?;
        Ok::<(), anyhow::Error>(())
    })?;

    // 4. Return the compile-time-embedded HTML card for this module
    info!("buzzer: registered /api/beep");
    Ok(CARD_HTML.to_string())
}
