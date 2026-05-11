// This file is the application entry point for the ESP-IDF-based modular HTTP server.
// - Connects to Wi-Fi using credentials from env.rs, then starts an EspHttpServer.
// - Delegates hardware control to seven submodules: led, servo, solar, buzzer, button, ultrasonic, and display.
// - Each submodule registers its own HTTP API endpoints and returns an HTML card snippet.
// - Assembles the final web page by injecting all module cards into the index.html template.
// - Serves the page on GET /, the stylesheet on GET /style.css, and a ping on GET /health.

mod button;
mod buzzer;
mod display;
mod led;
mod servo;
mod solar;
mod ultrasonic;

use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::hal::peripherals::Peripherals;
use esp_idf_svc::http::server::{Configuration as HttpServerConfig, EspHttpServer};
use esp_idf_svc::nvs::EspDefaultNvsPartition;
use esp_idf_svc::sys::*;
use esp_idf_svc::wifi::{
    BlockingWifi, ClientConfiguration, Configuration as WifiConfiguration, EspWifi, WifiEvent,
};
use log::info;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

include!("./env.rs");
// Environment variables are not committed to the repository for security reasons
// It should be structured as like this:
//
// pub const WIFI_SSID: &str = "<WIFI_NAME>";
// pub const WIFI_PASSWORD: &str = "<WIFI_PASSWORD>";

const INDEX_HTML: &str = include_str!("index.html");
const STYLE_CSS: &str = include_str!("style.css");

const USED_GPIOS: [i32; 13] = [2, 4, 5, 6, 7, 8, 9, 10, 11, 15, 18, 21, 22];

// Resets a single GPIO pin to a clean output state: disables hold, resets the pad,
// disables sleep-mode selection, sets floating pull mode, and maximises drive strength.
unsafe fn prepare_gpio_pad(pin: i32) {
    gpio_hold_dis(pin);
    gpio_reset_pin(pin);
    gpio_sleep_sel_dis(pin);
    gpio_set_pull_mode(pin, gpio_pull_mode_t_GPIO_FLOATING);
    gpio_set_drive_capability(pin, gpio_drive_cap_t_GPIO_DRIVE_CAP_3);
}

// Applies prepare_gpio_pad to every pin listed in USED_GPIOS, ensuring none of them
// retains stale state from a previous boot or deep-sleep cycle.
fn prepare_gpio_pads() {
    unsafe {
        for pin in USED_GPIOS {
            prepare_gpio_pad(pin);
        }
    }
}

/// Entry point. Initialises hardware, connects to Wi-Fi, registers all hardware
/// module handlers, assembles the HTML page, and parks the main task in an idle loop.
fn main() -> anyhow::Result<()> {
    // 1. Apply IDF patches required by the esp-idf-svc ecosystem
    esp_idf_svc::sys::link_patches();

    // 2. Initialise the ESP-IDF logger so info!/warn! output reaches the serial console
    esp_idf_svc::log::EspLogger::initialize_default();

    // 3. Prepare all used GPIO pads to ensure clean state for the drivers
    prepare_gpio_pads();

    info!("Starting ESP-IDF Rust web server...");

    // 4. Claim exclusive access to the peripheral singletons, event loop, and NVS storage
    let peripherals = Peripherals::take()?;
    let sys_loop = EspSystemEventLoop::take()?;
    let nvs = EspDefaultNvsPartition::take()?;

    // 5. Create the Wi-Fi driver bound to the modem peripheral.
    //    Clone the event loop before it is moved into BlockingWifi so we can
    //    subscribe to WifiEvent::StaDisconnected in the connect loop below.
    let event_loop = sys_loop.clone();
    let mut wifi = BlockingWifi::wrap(
        EspWifi::new(peripherals.modem, sys_loop.clone(), Some(nvs))?,
        sys_loop,
    )?;

    // 6. Configure as a station (client) with the credentials from env.rs
    wifi.set_configuration(&WifiConfiguration::Client(ClientConfiguration {
        ssid: WIFI_SSID.try_into().unwrap(),
        password: WIFI_PASSWORD.try_into().unwrap(),
        ..Default::default()
    }))?;

    // 7. Start the Wi-Fi stack (does not connect yet)
    wifi.start()?;
    info!("WiFi started");

    // 8. Attempt connection with automatic retry.
    let auth_failed = Arc::new(AtomicBool::new(false));
    let auth_failed_flag = auth_failed.clone();
    let _disconnect_sub = event_loop.subscribe::<WifiEvent, _>(move |event| {
        if matches!(event, WifiEvent::StaDisconnected { .. }) {
            auth_failed_flag.store(true, Ordering::Relaxed);
        }
    })?;

    loop {
        // 8a. Start a non-blocking association attempt
        auth_failed.store(false, Ordering::Relaxed);
        wifi.wifi_mut().connect()?;

        // 8b. Poll every 200 ms for success or fast failure.
        //     Without this, a rejected auth attempt would waste the full 15 s
        //     that BlockingWifi::connect() spends waiting for its internal timeout.
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        let associated = loop {
            if wifi.wifi().is_connected().unwrap_or(false) {
                break true; // Association + auth succeeded
            }
            if auth_failed.load(Ordering::Relaxed) {
                break false; // StaDisconnected fired — auth rejected by the AP
            }
            if std::time::Instant::now() >= deadline {
                break false; // Hard timeout (should rarely be reached)
            }
            std::thread::sleep(Duration::from_millis(200));
        };

        if !associated {
            log::warn!("WiFi association failed, retrying...");
            wifi.wifi_mut().disconnect().ok();
            // Short pause so the AP can clear its stale auth state.
            std::thread::sleep(Duration::from_millis(300));
            continue;
        }
        info!("WiFi associated, waiting for DHCP...");

        // 8c. Wait for the network interface (DHCP) to come up
        match wifi.wait_netif_up() {
            Ok(_) => break, // IP obtained — exit the retry loop
            Err(e) => {
                // DHCP timed out — disconnect and retry from the top
                log::warn!("WiFi DHCP failed ({:?}), retrying...", e);
                wifi.wifi_mut().disconnect().ok();
                std::thread::sleep(Duration::from_millis(300));
            }
        }
    }

    // 9. Log the assigned IP address
    let ip_info = wifi.wifi().sta_netif().get_ip_info()?;
    info!("WiFi connected!");
    info!("IP address: {}", ip_info.ip);
    info!("Open: http://{}", ip_info.ip);

    // 10. Create the HTTP server with default configuration
    let mut server = EspHttpServer::new(&HttpServerConfig::default())?;

    // 11. Register each hardware module; each function sets up its API endpoint
    //     and returns an HTML card to be embedded in the main page.
    let page_html = {
        // 11a. WS2812 addressable RGB LED on GPIO8 via RMT
        #[allow(deprecated)]
        let led_html = led::register(
            &mut server,
            peripherals.rmt.channel0,
            peripherals.pins.gpio8,
        )?;

        // 11b. SG90 micro servo on GPIO4 via LEDC PWM
        let servo_html = servo::register(
            &mut server,
            peripherals.ledc.timer0,
            peripherals.ledc.channel0,
            peripherals.pins.gpio4,
        )?;

        // 11c. Solar panel voltage via ADC1 on GPIO2
        let solar_html = solar::register(&mut server, peripherals.adc1, peripherals.pins.gpio2)?;

        // 11d. Button press counter on GPIO9 (onboard BOOT button — active-low, pull-up)
        //      GPIO9 is safe to use as a regular input after boot; the strapping
        //      level is only sampled by the ROM during the reset vector.
        let button_html = button::register(&mut server, peripherals.pins.gpio9)?;

        // 11e. Buzzer on GPIO15 via LEDC PWM
        //      NOTE: GPIO12 = USB D-, GPIO13 = USB D+ on ESP32-C6-DevKitC-1.
        //      Those pins must never be used as GPIO outputs — doing so drops
        //      the USB connection and resets the device.
        let buzzer_html = buzzer::register(
            &mut server,
            peripherals.ledc.timer1,
            peripherals.ledc.channel1,
            peripherals.pins.gpio15,
        )?;

        // 11f. HC-SR04 ultrasonic distance sensor — TRIG: GPIO22, ECHO: GPIO21
        let ultrasonic_html = ultrasonic::register(
            &mut server,
            peripherals.pins.gpio22,
            peripherals.pins.gpio21,
        )?;

        // 11g. ST7735S 128x160 SPI display via SPI2
        let display_html = display::register(
            &mut server,
            peripherals.spi2,
            peripherals.pins.gpio10,        // SCL
            peripherals.pins.gpio11,        // SDA
            peripherals.pins.gpio18.into(), // CS
            peripherals.pins.gpio5.into(),  // DC
            peripherals.pins.gpio6.into(),  // RST, RES
            peripherals.pins.gpio7.into(),  // BL, BLK
        )?;

        let modules_html = format!(
            "{}{}{}{}{}{}{}",
            led_html,
            servo_html,
            solar_html,
            button_html,
            buzzer_html,
            ultrasonic_html,
            display_html
        );

        Arc::new(INDEX_HTML.replace("{{MODULES}}", &modules_html))
    };

    // 12. Register the main page handler and the stylesheet handler, and a simple health check endpoint.
    {
        // 12a. Handler: GET / — serves the dynamically assembled main HTML page
        let page_for_handler = page_html.clone();
        server.fn_handler("/", esp_idf_svc::http::Method::Get, move |req| {
            // 1. Build response headers
            let headers = [("Content-Type", "text/html; charset=utf-8")];
            // 2. Open the response stream with HTTP 200
            let mut response = req.into_response(200, Some("OK"), &headers)?;
            // 3. Write the assembled HTML page
            response.write(page_for_handler.as_bytes())?;
            Ok::<(), anyhow::Error>(())
        })?;

        // 12b. Handler: GET /style.css — serves the stylesheet
        server.fn_handler("/style.css", esp_idf_svc::http::Method::Get, |req| {
            // 1. Build response headers
            let headers = [("Content-Type", "text/css; charset=utf-8")];
            // 2. Open the response stream with HTTP 200
            let mut response = req.into_response(200, Some("OK"), &headers)?;
            // 3. Write the embedded CSS
            response.write(STYLE_CSS.as_bytes())?;
            Ok::<(), anyhow::Error>(())
        })?;

        // 12c. Handler: GET /health — simple health-check endpoint
        server.fn_handler("/health", esp_idf_svc::http::Method::Get, |req| {
            // 1. Build response headers
            let headers = [("Content-Type", "application/json")];
            // 2. Open the response stream with HTTP 200
            let mut response = req.into_response(200, Some("OK"), &headers)?;
            // 3. Write the JSON status payload
            response.write(br#"{"status":"ok"}"#)?;
            Ok::<(), anyhow::Error>(())
        })?;
    }

    info!("HTTP server started");

    info!("Let's goooo!");

    // 13. Park the main task in an idle loop.
    //     The HTTP server runs on background threads managed by ESP-IDF; the main task
    //     must stay alive to keep the server (and the wifi variable) from being dropped.
    loop {
        std::thread::sleep(Duration::from_secs(60));
    }
}
