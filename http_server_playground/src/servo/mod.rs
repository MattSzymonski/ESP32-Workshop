// This file implements the SG90 servo motor module for the HTTP server.
// - Controls a micro servo on GPIO4 via ESP32 LEDC PWM at 50 Hz with 14-bit resolution.
// - Registers GET /api/servo?angle=<0-180>: moves the servo to the requested angle.
// - Converts the angle to a PWM duty cycle in the 0.5 ms–2.5 ms SG90 pulse-width range.
// - Timer and channel drivers are kept alive in Arc<Mutex<>> for the server's lifetime.
// - Depends on: esp-idf-svc (LEDC, GPIO, HTTP server).

use embedded_io::Write;
use esp_idf_svc::hal::gpio::OutputPin;
use esp_idf_svc::hal::ledc::{
    config::TimerConfig, LedcChannel, LedcDriver, LedcTimer, LedcTimerDriver, Resolution,
};
use esp_idf_svc::hal::units::Hertz;
use esp_idf_svc::http::server::EspHttpServer;
use std::sync::{Arc, Mutex};

const CARD_HTML: &str = include_str!("card.html");

/// Registers the SG90 servo control endpoint on the server and returns the HTML card
/// to be embedded in the main page's center column.
///
/// Endpoint: `GET /api/servo?angle=<0-180>`
/// Moves the servo to the requested angle and holds it there.
///
/// Hardware: Tower Pro Micro Servo 9g SG90 — connect signal wire to GPIO6.
/// Signal spec: 50 Hz PWM, 0.5 ms pulse = 0°, 2.5 ms pulse = 180°, using LEDC 14-bit resolution.
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
    // 1. Configure the LEDC timer: 50 Hz, 14-bit resolution
    //    14-bit gives 16383 ticks per 20 ms period for precise sub-millisecond control
    let timer_driver = LedcTimerDriver::new(
        timer,
        &TimerConfig::new()
            .frequency(Hertz(50))
            .resolution(Resolution::Bits14),
    )?;

    // 2. Create the LEDC PWM channel bound to the servo signal pin.
    //    Both the timer driver and the channel driver are bundled into a single Arc<Mutex<>>
    //    so that the timer driver is kept alive for the lifetime of the servo
    //    (dropping it would call ledc_timer_rst and stop the PWM signal).
    let driver = LedcDriver::new(channel, &timer_driver, pin)?;
    let servo = Arc::new(Mutex::new((timer_driver, driver)));

    // 3. Register the HTTP endpoint
    server.fn_handler(
        "/api/servo",
        esp_idf_svc::http::Method::Get,
        move |req| {
            // 3a. Parse ?angle=N from the request URI (defaults to 90, clamped to 0-180)
            let angle: u32 = req
                .uri()
                .split_once('?')
                .and_then(|(_, q)| {
                    q.split('&')
                        .find(|p| p.starts_with("angle="))
                        .and_then(|p| p["angle=".len()..].parse::<u32>().ok())
                })
                .unwrap_or(90)
                .min(180);

            // 3b. Convert angle to LEDC duty count for 14-bit / 50 Hz
            //     Period = 20 ms, resolution = 16383 ticks
            //     SG90 full range: 0.5 ms (0°) to 2.5 ms (180°)
            //     min_duty = 16383 * 0.5 / 20 ~= 410
            //     max_duty = 16383 * 2.5 / 20 ~= 2048
            const MIN_DUTY: u32 = 410;
            const MAX_DUTY: u32 = 2048;
            let duty = MIN_DUTY + (angle * (MAX_DUTY - MIN_DUTY)) / 180;

            // 3c. Apply the duty to move the servo to the target angle
            servo.lock().unwrap().1.set_duty(duty)?;

            // 3d. Send JSON confirmation response
            let body = format!(r#"{{"angle":{},"duty":{}}}"#, angle, duty);
            let headers = [("Content-Type", "application/json")];
            let mut response = req.into_response(200, Some("OK"), &headers)?;
            response.write(body.as_bytes())?;
            Ok::<(), anyhow::Error>(())
        },
    )?;

    // 4. Return the compile-time-embedded HTML card for this module
    Ok(CARD_HTML.to_string())
}
