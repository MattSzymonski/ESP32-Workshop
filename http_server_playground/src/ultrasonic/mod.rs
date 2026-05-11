// This file implements the HC-SR04 ultrasonic distance sensor module for the HTTP server.
// - Triggers the sensor on GPIO22 (TRIG) with a 10 µs pulse.
// - Measures the duration of the HIGH response pulse on GPIO21 (ECHO).
// - Timing uses esp_timer_get_time() (free-running 64-bit µs counter) instead of Instant
//   to avoid FreeRTOS task-preemption inflating the measured pulse width.
// - Calculates distance: d = pulse_duration_µs × 0.0343 / 2  (cm, speed of sound 343 m/s at 20 °C).
// - Out-of-range readings (< 2 cm or > 400 cm) are rejected as sensor noise.
// - Registers GET /api/ultrasonic: returns distance_cm and raw echo_us, or an error string.
// - Measurement is protected by an Arc<Mutex<>> so concurrent requests queue rather than overlap.
// - Depends on: esp-idf-svc (GPIO, HTTP server).

use embedded_io::Write;
use esp_idf_svc::hal::gpio::{Gpio21, Gpio22, Input, Output, PinDriver, Pull};
use esp_idf_svc::http::server::EspHttpServer;
use log::info;
use std::sync::{Arc, Mutex};

const CARD_HTML: &str = include_str!("card.html");

// HC-SR04 timing constants
const TRIG_PULSE_US: u64 = 10; // trigger pulse width (µs)
const ECHO_TIMEOUT_US: u64 = 30_000; // 30 ms — covers up to ~5 m

// Valid distance window — anything outside this is sensor noise / multiple reflection
const MIN_DISTANCE_CM: f32 = 2.0;
const MAX_DISTANCE_CM: f32 = 400.0;

// Speed of sound at ~20 °C in cm/µs
const SOUND_CM_PER_US: f32 = 0.0343;

/// Registers the ultrasonic distance sensor endpoint on the server and returns the HTML card.
///
/// Endpoint: `GET /api/ultrasonic`
/// Returns `{"distance_cm": <f32>, "echo_us": <u64>}` on success,
/// or `{"error": "no echo"}` if no pulse is received within the timeout.
pub fn register(
    server: &mut EspHttpServer,
    trig_pin: Gpio22,
    echo_pin: Gpio21,
) -> anyhow::Result<String> {
    // 1. Transmute peripheral lifetimes to 'static so the drivers can be moved
    //    into the HTTP handler closure. Safe because ESP32 peripherals live for
    //    the entire program lifetime.
    let trig_pin: Gpio22<'static> = unsafe { core::mem::transmute(trig_pin) };
    let echo_pin: Gpio21<'static> = unsafe { core::mem::transmute(echo_pin) };

    // 2. Initialise GPIO drivers.
    //    TRIG: active-high output, starts LOW.
    //    ECHO: input with no pull (the HC-SR04 drives the line itself).
    let trig: PinDriver<'static, Output> = PinDriver::output(trig_pin)?;
    let echo: PinDriver<'static, Input> = PinDriver::input(echo_pin, Pull::Floating)?;

    // 3. Wrap in Arc<Mutex<>> so concurrent HTTP requests queue rather than overlap.
    let sensor = Arc::new(Mutex::new((trig, echo)));

    // 4. Register the HTTP endpoint
    server.fn_handler(
        "/api/ultrasonic",
        esp_idf_svc::http::Method::Get,
        move |req| {
            let result = measure(&mut *sensor.lock().unwrap());

            let mut response =
                req.into_response(200, Some("OK"), &[("Content-Type", "application/json")])?;
            let json = match result {
                Ok((distance_cm, echo_us)) => format!(
                    r#"{{"distance_cm":{:.1},"echo_us":{}}}"#,
                    distance_cm, echo_us
                ),
                Err(reason) => format!(r#"{{"error":"{}"}}"#, reason),
            };
            response.write(json.as_bytes())?;
            Ok::<(), anyhow::Error>(())
        },
    )?;

    // 5. Return the compile-time-embedded HTML card for this module
    info!("ultrasonic: registered /api/ultrasonic");
    Ok(CARD_HTML.to_string())
}

/// Performs a single HC-SR04 measurement.
/// Returns `Ok((distance_cm, echo_duration_us))` or `Err(reason_string)`.
fn measure(
    sensor: &mut (PinDriver<'static, Output>, PinDriver<'static, Input>),
) -> Result<(f32, u64), &'static str> {
    let (trig, echo) = sensor;

    // 4a. Guard: if ECHO is already HIGH from a previous measurement that was not
    //     cleaned up, wait for it to go LOW before triggering again.
    let guard_start = timer_us();
    while echo.is_high() {
        if timer_us().saturating_sub(guard_start) > ECHO_TIMEOUT_US {
            return Err("echo stuck high");
        }
    }

    // 4b. Send 10 µs trigger pulse
    trig.set_high().map_err(|_| "trig error")?;
    busy_wait_us(TRIG_PULSE_US);
    trig.set_low().map_err(|_| "trig error")?;

    // 4c. Wait for ECHO to go HIGH (start of echo pulse).
    //     Uses esp_timer_get_time() — a free-running 64-bit µs counter — so that
    //     FreeRTOS preempting this task between loop iterations does not inflate
    //     the measured duration the way Instant::elapsed() would.
    let wait_start = timer_us();
    while echo.is_low() {
        if timer_us().saturating_sub(wait_start) > ECHO_TIMEOUT_US {
            return Err("no echo");
        }
    }

    // 4d. Measure how long ECHO stays HIGH using the same µs counter
    let echo_start = timer_us();
    while echo.is_high() {
        if timer_us().saturating_sub(echo_start) > ECHO_TIMEOUT_US {
            return Err("echo timeout");
        }
    }
    let echo_us = timer_us().saturating_sub(echo_start);

    // 4e. Calculate and validate distance
    let distance_cm = echo_us as f32 * SOUND_CM_PER_US / 2.0;
    if distance_cm < MIN_DISTANCE_CM || distance_cm > MAX_DISTANCE_CM {
        return Err("out of range");
    }

    Ok((distance_cm, echo_us))
}

/// Returns the current value of the ESP-IDF free-running 64-bit microsecond timer.
/// This counter is never reset by FreeRTOS task scheduling, so it gives accurate
/// wall-clock µs even if the calling task is preempted between two reads.
fn timer_us() -> u64 {
    unsafe { esp_idf_svc::sys::esp_timer_get_time() as u64 }
}

/// Busy-wait for approximately `us` microseconds using `esp_rom_delay_us`.
/// Used only for the 10 µs trigger pulse where thread::sleep granularity is too coarse.
fn busy_wait_us(us: u64) {
    unsafe { esp_idf_svc::sys::esp_rom_delay_us(us as u32) };
}
