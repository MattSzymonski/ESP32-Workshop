// This file is the application entry point for the ESP-IDF-based modular HTTP server.
// - Connects to Wi-Fi using credentials from env.rs, then starts an EspHttpServer.
// - Delegates hardware control to eight submodules: led, servo, solar, buzzer, button, ultrasonic, joystick, and display.
// - Each submodule registers its own HTTP API endpoints and returns an HTML card snippet.
// - Assembles the final web page by injecting all module cards into the index.html template.
// - Serves the page on GET /, the stylesheet on GET /style.css, and a ping on GET /health.

mod mem;

#[cfg(feature = "button")]
mod button;
#[cfg(feature = "buzzer")]
mod buzzer;
#[cfg(feature = "display")]
mod display;
#[cfg(feature = "gamepad")]
mod gamepad;
#[cfg(feature = "joystick")]
mod joystick;
#[cfg(feature = "led")]
mod led;
#[cfg(feature = "servo")]
mod servo;
#[cfg(feature = "solar")]
mod solar;
#[cfg(feature = "ultrasonic")]
mod ultrasonic;

use ::log::info;
use esp_idf_svc::eventloop::EspSystemEventLoop;
#[cfg(any(feature = "solar", feature = "joystick"))]
use esp_idf_svc::hal::adc::oneshot::AdcDriver as OneshotAdcDriver;
#[cfg(any(feature = "solar", feature = "joystick"))]
use esp_idf_svc::hal::adc::{ADC1, ADCU1};
use esp_idf_svc::hal::peripherals::Peripherals;
use esp_idf_svc::http::server::{Configuration as HttpServerConfig, EspHttpServer};
use esp_idf_svc::nvs::EspDefaultNvsPartition;
use esp_idf_svc::sys::*;
use esp_idf_svc::wifi::{
    BlockingWifi, ClientConfiguration, Configuration as WifiConfiguration, EspWifi, WifiEvent,
};
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

const USED_GPIOS: [i32; 16] = [0, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 15, 18, 21, 20, 22];

/// The main page's components, kept as separate slices/strings so we never
/// need to materialise the full ~57 KiB page in heap. The `prefix` and
/// `suffix` are zero-cost slices into the flash-resident `INDEX_HTML`; the
/// `cards` vector holds one heap-allocated `Arc<str>` per module (each
/// 1–10 KiB). Cloning a `PageParts` is cheap — it just bumps the `Arc`.
#[derive(Clone)]
struct PageParts {
    prefix: &'static str,
    suffix: &'static str,
    cards: Arc<Vec<Arc<str>>>,
}

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
    mem::report("boot");

    // 4. Claim exclusive access to the peripheral singletons, event loop, and NVS storage
    let peripherals = Peripherals::take()?;
    let sys_loop = EspSystemEventLoop::take()?;
    let nvs = EspDefaultNvsPartition::take()?;
    mem::report("after peripherals/nvs");

    // 4b. Initialise the BLE controller BEFORE Wi-Fi (only when `gamepad` is enabled).
    //
    //     Why this is mandatory on ESP32-C6:
    //     The BLE controller's link-layer scheduler tables, ACL TX/RX buffers,
    //     and event queues need a single contiguous ~30 KiB DRAM allocation at
    //     init time. Wi-Fi grabs ~80 KiB of contiguous DRAM the moment
    //     `EspWifi::new` is called, so if BLE is initialised AFTER Wi-Fi the
    //     largest free block is too small and `r_ble_controller_init` fails
    //     with a `r_ble_lll_env_deinit` assertion (the cleanup path runs on
    //     half-initialised state and trips an internal sanity check).
    //
    //     `BLEDevice::take()` is a `OnceCell` initializer; calling it here
    //     just brings up the controller and host. The actual scan/connect
    //     state machine is still started later from `gamepad_handle.start()`.
    #[cfg(feature = "gamepad")]
    {
        let _ = esp32_nimble::BLEDevice::take();
        mem::report("after BLE controller init");
    }

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
            ::log::warn!("WiFi association failed, retrying...");
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
                ::log::warn!("WiFi DHCP failed ({:?}), retrying...", e);
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
    mem::report("after WiFi up");

    // 10. Create the shared ADC1 driver (only if any module that uses it is enabled).
    //     ADC1 is a singleton peripheral; both solar and joystick need it, so
    //     we create one AdcDriver here and share it via Arc.
    #[cfg(any(feature = "solar", feature = "joystick"))]
    let adc1_driver = {
        let adc1: ADC1<'static> = unsafe { core::mem::transmute(peripherals.adc1) };
        Arc::new(OneshotAdcDriver::<ADCU1>::new(adc1)?)
    };

    // 11. Create the HTTP server with default configuration
    let mut server = EspHttpServer::new(&HttpServerConfig {
        max_uri_handlers: 20,
        ..Default::default()
    })?;
    mem::report("after HTTP server");

    // 11. Register each enabled hardware module; each function sets up its API
    //     endpoint and returns an HTML card to be embedded in the main page.
    //     We push each card into `cards` immediately so we never need to keep
    //     all of them as named locals (lets the compiler drop temporaries
    //     promptly on tight DRAM).
    let page_html = {
        let mut cards: Vec<Arc<str>> = Vec::with_capacity(9);

        // 11a. WS2812 addressable RGB LED on GPIO8 via RMT
        #[cfg(feature = "led")]
        {
            #[allow(deprecated)]
            let led_html = led::register(
                &mut server,
                peripherals.rmt.channel0,
                peripherals.pins.gpio8,
            )?;
            cards.push(led_html.into());
        }

        // 11b. SG90 micro servo on GPIO4 via LEDC PWM
        #[cfg(feature = "servo")]
        {
            let servo_html = servo::register(
                &mut server,
                peripherals.ledc.timer0,
                peripherals.ledc.channel0,
                peripherals.pins.gpio4,
            )?;
            cards.push(servo_html.into());
        }

        // 11c. Solar panel voltage via ADC1 on GPIO2 (shares ADC1 driver with joystick)
        #[cfg(feature = "solar")]
        {
            let solar_html =
                solar::register(&mut server, adc1_driver.clone(), peripherals.pins.gpio2)?;
            cards.push(solar_html.into());
        }

        // 11d. Button press counter on GPIO9 (onboard BOOT button — active-low, pull-up)
        //      GPIO9 is safe to use as a regular input after boot; the strapping
        //      level is only sampled by the ROM during the reset vector.
        #[cfg(feature = "button")]
        {
            let button_html = button::register(&mut server, peripherals.pins.gpio9)?;
            cards.push(button_html.into());
        }

        // 11e. Buzzer on GPIO15 via LEDC PWM
        //      NOTE: GPIO12 = USB D-, GPIO13 = USB D+ on ESP32-C6-DevKitC-1.
        //      Those pins must never be used as GPIO outputs — doing so drops
        //      the USB connection and resets the device.
        #[cfg(feature = "buzzer")]
        {
            let buzzer_html = buzzer::register(
                &mut server,
                peripherals.ledc.timer1,
                peripherals.ledc.channel1,
                peripherals.pins.gpio15,
            )?;
            cards.push(buzzer_html.into());
        }

        // 11f. HC-SR04 ultrasonic distance sensor — TRIG: GPIO22, ECHO: GPIO21
        #[cfg(feature = "ultrasonic")]
        {
            let ultrasonic_html = ultrasonic::register(
                &mut server,
                peripherals.pins.gpio22,
                peripherals.pins.gpio21,
            )?;
            cards.push(ultrasonic_html.into());
        }

        // 11g. Joystick — VRX: GPIO0, VRY: GPIO3 (ADC1, shared driver), SW: GPIO20
        //      Note: GPIO19/20 have no ADC on ESP32-C6; GPIO0/GPIO3 are the free ADC1 pins.
        //      Note: GPIO15 is the buzzer; GPIO20 is used for SW instead.
        #[cfg(feature = "joystick")]
        {
            let joystick_html = joystick::register(
                &mut server,
                adc1_driver.clone(),
                peripherals.pins.gpio0,
                peripherals.pins.gpio3,
                peripherals.pins.gpio20,
            )?;
            cards.push(joystick_html.into());
        }

        // 11h. ST7735S 128x160 SPI display via SPI2
        #[cfg(feature = "display")]
        {
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
            cards.push(display_html.into());
        }

        // 11i. BLE HID gamepad (e.g. 8BitDo Ultimate 2C in BT mode).
        //      Uses no GPIOs — talks to the controller over the on-board BLE radio
        //      via the esp32-nimble crate. Coexists with Wi-Fi on the ESP32-C6 modem.
        //      We only register the HTTP endpoint here and obtain a handle; the
        //      BLE state machine is started later, AFTER the page parts are built,
        //      because NimBLE's first scan grabs several KiB of mbufs and easily
        //      fragments the heap.
        #[cfg(feature = "gamepad")]
        let gamepad_handle = {
            let (gamepad_html, handle) = gamepad::register(&mut server)?;
            cards.push(gamepad_html.into());
            handle
        };

        mem::report("after all modules registered");

        // Build the page WITHOUT ever materialising it as a single big String.
        //
        // The naive approach — `format!()` of all cards into `modules_html`
        // followed by `INDEX_HTML.replace("{{MODULES}}", ...)` — needs ~85 KiB
        // of contiguous heap (28 KiB for the joined card text, plus 57 KiB for
        // the rendered page, both held simultaneously during `replace`).
        // After Wi-Fi + display + render-thread stacks the largest free block
        // is only ~76 KiB, so the alloc fails.
        //
        // Instead: split the template once at startup (zero-copy slice), keep
        // each card's HTML as its own `Arc<str>`, and stream them in the
        // handler — prefix → cards → suffix. Peak transient allocation: 0.
        const PLACEHOLDER: &str = "{{MODULES}}";
        let split = INDEX_HTML
            .split_once(PLACEHOLDER)
            .expect("index.html must contain {{MODULES}}");
        let page = PageParts {
            prefix: split.0,
            suffix: split.1,
            cards: Arc::new(cards),
        };
        mem::report("after page assembly (streamed, no big alloc)");

        // Page is decomposed and owned by Arcs — safe to bring up BLE.
        #[cfg(feature = "gamepad")]
        {
            gamepad_handle.start();
            mem::report("after gamepad BLE thread spawn");
        }

        page
    };

    // 12. Register the main page handler and the stylesheet handler, and a simple health check endpoint.
    {
        // 12a. Handler: GET / — streams the page in three parts (prefix, cards, suffix)
        //       so we never need a single large contiguous allocation at runtime.
        let page_for_handler = page_html.clone();
        server.fn_handler("/", esp_idf_svc::http::Method::Get, move |req| {
            let headers = [("Content-Type", "text/html; charset=utf-8")];
            let mut response = req.into_response(200, Some("OK"), &headers)?;
            // Stream the static template prefix from flash.
            response.write(page_for_handler.prefix.as_bytes())?;
            // Stream each module card in turn — each chunk is freed implicitly
            // by the response writer before the next one is sent.
            for card in page_for_handler.cards.iter() {
                response.write(card.as_bytes())?;
            }
            // Stream the static template suffix from flash.
            response.write(page_for_handler.suffix.as_bytes())?;
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
