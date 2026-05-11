// This file implements the solar panel voltage module for the HTTP server.
// - Reads the solar panel output voltage via the ESP32-C6 ADC1 peripheral on GPIO2.
// - Registers GET /api/solar: returns a 12-bit raw reading and the corresponding voltage in mV.
// - GPIO2 pull-down is enabled in software so a floating (unconnected) pin reads ≈ 0 mV.
// - Accepts a shared Arc<AdcDriver> so ADC1 can be shared with other modules (e.g. joystick).
// - Depends on: esp-idf-svc (ADC oneshot, GPIO, HTTP server).

use esp_idf_svc::hal::adc::attenuation::DB_12;
use esp_idf_svc::hal::adc::oneshot::config::AdcChannelConfig;
use esp_idf_svc::hal::adc::oneshot::{AdcChannelDriver, AdcDriver};
use esp_idf_svc::hal::adc::ADCU1;
use esp_idf_svc::hal::gpio::Gpio2;
use esp_idf_svc::http::server::EspHttpServer;
use log::info;
use std::sync::{Arc, Mutex};

const CARD_HTML: &str = include_str!("card.html");

// Full-scale voltage in mV at DB_12 attenuation (≈ 3.3 V).
const VREF_MV: u32 = 3300;
const ADC_MAX: u32 = 4095;

/// Registers the solar panel ADC endpoint on the server and returns the HTML card
/// to be embedded in the main page's center column.
///
/// Endpoint: `GET /api/solar`
/// Returns the raw 12-bit ADC reading and the calculated voltage in millivolts.
pub fn register(
    server: &mut EspHttpServer,
    adc_driver: Arc<AdcDriver<'static, ADCU1>>,
    pin: Gpio2,
) -> anyhow::Result<String> {
    // 1. Enable the internal pull-down on GPIO2 (~45 kΩ to GND).
    //    Without this, a floating pin picks up capacitive noise and returns
    //    spurious readings of 400–1200 mV when no panel is connected.
    //    A connected panel's output easily overrides the weak pull-down.
    unsafe { esp_idf_svc::sys::gpio_pulldown_en(2) };

    // 2. Transmute the pin lifetime to 'static so it can be moved into the
    //    'static HTTP handler closure. The ADC1 driver is already 'static
    //    (created and transmuted in main.rs).
    let pin: Gpio2<'static> = unsafe { core::mem::transmute(pin) };

    // 3. Configure the ADC channel on GPIO2.
    //    adc_driver.clone() passes a second Arc reference into AdcChannelDriver
    //    so the driver stays alive as long as either the channel or the closure holds it.
    let channel_config = AdcChannelConfig {
        attenuation: DB_12, // 0–3.3 V input range
        ..Default::default()
    };
    let channel = AdcChannelDriver::new(adc_driver.clone(), pin, &channel_config)?;
    let channel = Arc::new(Mutex::new(channel));

    // 4. Register the HTTP endpoint
    server.fn_handler("/api/solar", esp_idf_svc::http::Method::Get, move |req| {
        // 4a. Take a single ADC reading
        let raw: u16 = adc_driver.read(&mut channel.lock().unwrap())?;

        // 4b. Convert raw count to millivolts (linear mapping, 12-bit full scale = 3300 mV)
        let voltage_mv = (raw as u32 * VREF_MV) / ADC_MAX;

        // 4c. Send JSON response
        let mut response =
            req.into_response(200, Some("OK"), &[("Content-Type", "application/json")])?;
        let json = format!(r#"{{"raw":{},"voltage_mv":{}}}"#, raw, voltage_mv);
        response.write(json.as_bytes())?;
        Ok::<(), anyhow::Error>(())
    })?;

    // 5. Return the compile-time-embedded HTML card for this module
    info!("solar: registered /api/solar");
    Ok(CARD_HTML.to_string())
}
