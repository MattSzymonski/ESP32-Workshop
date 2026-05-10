// This file is the application entry point for the ESP-IDF-based modular HTTP server.
// - Connects to Wi-Fi using credentials from env.rs, then starts an EspHttpServer.
// - Delegates hardware control to three submodules: led, servo, and display.
// - Each submodule registers its own HTTP API endpoints and returns an HTML card snippet.
// - Assembles the final web page by injecting all module cards into the index.html template.
// - Serves the page on GET /, the stylesheet on GET /style.css, and a ping on GET /health.
// - Depends on: esp-idf-svc (Wi-Fi, HTTP server, peripherals), anyhow, and the three submodules.

mod display;
mod led;
mod renderer;
mod servo;

use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::hal::peripherals::Peripherals;
use esp_idf_svc::http::server::{Configuration as HttpServerConfig, EspHttpServer};
use esp_idf_svc::nvs::EspDefaultNvsPartition;
use esp_idf_svc::sys::*;
use esp_idf_svc::wifi::{
    BlockingWifi, ClientConfiguration, Configuration as WifiConfiguration, EspWifi,
};
use log::info;
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

const USED_GPIOS: [i32; 8] = [4, 5, 6, 7, 8, 10, 11, 18];

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

    prepare_gpio_pads();

    info!("Starting ESP-IDF Rust web server...");

    // 3. Claim exclusive access to the peripheral singletons, event loop, and NVS storage
    let peripherals = Peripherals::take()?;
    let sys_loop = EspSystemEventLoop::take()?;
    let nvs = EspDefaultNvsPartition::take()?;

    // 4. Create the Wi-Fi driver bound to the modem peripheral
    let mut wifi = BlockingWifi::wrap(
        EspWifi::new(peripherals.modem, sys_loop.clone(), Some(nvs))?,
        sys_loop,
    )?;

    // 5. Configure as a station (client) with the credentials from env.rs
    wifi.set_configuration(&WifiConfiguration::Client(ClientConfiguration {
        ssid: WIFI_SSID.try_into().unwrap(),
        password: WIFI_PASSWORD.try_into().unwrap(),
        ..Default::default()
    }))?;

    // 6. Start the Wi-Fi stack (does not connect yet)
    wifi.start()?;
    info!("WiFi started");

    // Give the router time to expire any stale association from a previous run,
    // reducing the number of AUTH_EXPIRE retries on fast restarts.
    std::thread::sleep(Duration::from_millis(500));

    // 7. Attempt connection with automatic retry.
    //    The router can reject the first attempt with AUTH_EXPIRE when it still holds
    //    stale state from a previous run, so we loop until both the association and
    //    DHCP lease succeed.
    loop {
        // 7a. Try to associate with the access point
        match wifi.connect() {
            Ok(_) => {}
            Err(e) => {
                // Association failed — wait briefly and retry from the top
                log::warn!("WiFi connect failed ({:?}), retrying...", e);
                std::thread::sleep(Duration::from_secs(2));
                continue;
            }
        }
        info!("WiFi connecting...");

        // 7b. Wait for the network interface (DHCP) to come up
        match wifi.wait_netif_up() {
            Ok(_) => break, // Connected successfully — exit the retry loop
            Err(e) => {
                // DHCP timed out — disconnect to reset driver state before retrying
                log::warn!("WiFi netif not up ({:?}), retrying...", e);
                wifi.disconnect().ok();
                std::thread::sleep(Duration::from_secs(2));
            }
        }
    }

    // 8. Log the assigned IP address
    let ip_info = wifi.wifi().sta_netif().get_ip_info()?;
    info!("WiFi connected!");
    info!("IP address: {}", ip_info.ip);
    info!("Open: http://{}", ip_info.ip);

    // 9. Create the HTTP server with default configuration
    let mut server = EspHttpServer::new(&HttpServerConfig::default())?;

    // 10. Register each hardware module; each function sets up its API endpoint
    //     and returns an HTML card to be embedded in the main page.
    // 10a. WS2812 addressable RGB LED on GPIO8 via RMT
    #[allow(deprecated)]
    let led_html = led::register(
        &mut server,
        peripherals.rmt.channel0,
        peripherals.pins.gpio8,
    )?;

    // 10b. SG90 micro servo on GPIO4 via LEDC PWM
    let servo_html = servo::register(
        &mut server,
        peripherals.ledc.timer0,
        peripherals.ledc.channel0,
        peripherals.pins.gpio4,
    )?;

    // 10c. ST7735S 128x160 SPI display via SPI2
    // let display_html = display::register(
    //     &mut server,
    //     peripherals.spi2,
    //     peripherals.pins.gpio10,        // SCL
    //     peripherals.pins.gpio11,        // SDA
    //     peripherals.pins.gpio18.into(), // CS
    //     peripherals.pins.gpio5.into(),  // DC
    //     peripherals.pins.gpio6.into(),  // RST, RES
    //     peripherals.pins.gpio7.into(),  // BL, BLK
    // )?;

    // 10d. Software 3-D renderer — rotating cube on the same ST7735S via SPI3
    // Note: renderer owns its own SPI bus instance (SPI3 / HSPI) with the same pins.
    // To avoid bus contention, do not use the display module and renderer simultaneously.
    let renderer_html = renderer::register(
        &mut server,
        peripherals.spi2,
        peripherals.pins.gpio10,        // SCL
        peripherals.pins.gpio11,        // SDA
        peripherals.pins.gpio18.into(), // CS
        peripherals.pins.gpio5.into(),  // DC
        peripherals.pins.gpio6.into(),  // RST, RES
        peripherals.pins.gpio7.into(),  // BL, BLK
    )?;

    // 11. Build the final index page by substituting all module cards into the template
    // let modules_html = format!(
    //     "{}{}{}{}",
    //     led_html, servo_html, display_html, renderer_html
    // );
    let modules_html = format!("{}{}{}", led_html, servo_html, renderer_html);

    let page_html = Arc::new(INDEX_HTML.replace("{{MODULES}}", &modules_html));

    // Handler: GET / — serves the dynamically assembled main HTML page
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

    // Handler: GET /style.css — serves the stylesheet
    server.fn_handler("/style.css", esp_idf_svc::http::Method::Get, |req| {
        // 1. Build response headers
        let headers = [("Content-Type", "text/css; charset=utf-8")];
        // 2. Open the response stream with HTTP 200
        let mut response = req.into_response(200, Some("OK"), &headers)?;
        // 3. Write the embedded CSS
        response.write(STYLE_CSS.as_bytes())?;
        Ok::<(), anyhow::Error>(())
    })?;

    // Handler: GET /health — simple health-check endpoint
    server.fn_handler("/health", esp_idf_svc::http::Method::Get, |req| {
        // 1. Build response headers
        let headers = [("Content-Type", "application/json")];
        // 2. Open the response stream with HTTP 200
        let mut response = req.into_response(200, Some("OK"), &headers)?;
        // 3. Write the JSON status payload
        response.write(br#"{"status":"ok"}"#)?;
        Ok::<(), anyhow::Error>(())
    })?;

    info!("HTTP server started");

    // 12. Park the main task in an idle loop.
    //     The HTTP server runs on background threads managed by ESP-IDF; the main task
    //     must stay alive to keep the server (and the wifi variable) from being dropped.
    loop {
        std::thread::sleep(Duration::from_secs(60));
    }
}
