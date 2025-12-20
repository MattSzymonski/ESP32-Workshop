mod camera;

use embedded_io::Write;
use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::hal::peripherals::Peripherals;
use esp_idf_svc::http::server::{Configuration as HttpConfig, EspHttpServer};
use esp_idf_svc::nvs::EspDefaultNvsPartition;
use esp_idf_svc::wifi::{BlockingWifi, ClientConfiguration, Configuration, EspWifi};
use log::info;
use std::sync::{Arc, Mutex};

// Credentials is not committed to the repository for security reasons
// it should define WIFI_SSID and WIFI_PASSWORD constants:
include!("./credentials.rs");
//pub const WIFI_SSID: &str = "<WIFI_NAME>";
//pub const WIFI_PASSWORD: &str = "<WIFI_PASSWORD>";

fn main() -> anyhow::Result<()> {
    esp_idf_svc::sys::link_patches();
    esp_idf_svc::log::EspLogger::initialize_default();

    info!("Starting camera web server...");
    let peripherals = Peripherals::take().unwrap();
    let sys_loop = EspSystemEventLoop::take()?;
    let nvs = EspDefaultNvsPartition::take()?;

    // Connect to WiFi
    let mut wifi = BlockingWifi::wrap(
        EspWifi::new(peripherals.modem, sys_loop.clone(), Some(nvs))?,
        sys_loop,
    )?;

    wifi.set_configuration(&Configuration::Client(ClientConfiguration {
        ssid: WIFI_SSID.try_into().unwrap(),
        password: WIFI_PASSWORD.try_into().unwrap(),
        ..Default::default()
    }))?;

    wifi.start()?;
    info!("WiFi started, connecting...");
    wifi.connect()?;
    wifi.wait_netif_up()?;

    let ip_info = wifi.wifi().sta_netif().get_ip_info()?;
    info!("Connected! IP: {}", ip_info.ip);

    // Initialize the camera with XIAO ESP32-S3 Sense pinout
    let cam = camera::Camera::new(
        peripherals.pins.gpio10, // XCLK
        peripherals.pins.gpio15, // D0
        peripherals.pins.gpio17, // D1
        peripherals.pins.gpio18, // D2
        peripherals.pins.gpio16, // D3
        peripherals.pins.gpio14, // D4
        peripherals.pins.gpio12, // D5
        peripherals.pins.gpio11, // D6
        peripherals.pins.gpio48, // D7
        peripherals.pins.gpio38, // VSYNC
        peripherals.pins.gpio47, // HREF
        peripherals.pins.gpio13, // PCLK
        peripherals.pins.gpio40, // SIOD (SDA)
        peripherals.pins.gpio39, // SIOC (SCL)
        esp_idf_sys::camera::pixformat_t_PIXFORMAT_JPEG,
        esp_idf_sys::camera::framesize_t_FRAMESIZE_VGA, // Smaller for faster transfer
        esp_idf_sys::camera::camera_fb_location_t_CAMERA_FB_IN_PSRAM,
    )?;
    info!("Camera initialized!");

    let cam = Arc::new(Mutex::new(cam));

    // Start HTTP server
    let mut server = EspHttpServer::new(&HttpConfig::default())?;

    // Serve camera image at /capture
    let cam_capture = cam.clone();
    server.fn_handler("/capture", esp_idf_svc::http::Method::Get, move |req| {
        let cam = cam_capture.lock().unwrap();
        if let Some(fb) = cam.get_framebuffer() {
            let data = fb.data();
            let headers = [("Content-Type", "image/jpeg")];
            let mut response = req.into_response(200, Some("OK"), &headers)?;
            response.write_all(data)?;
        } else {
            req.into_status_response(500)?;
        }
        Ok::<(), anyhow::Error>(())
    })?;

    // Serve HTML page at /
    server.fn_handler("/", esp_idf_svc::http::Method::Get, |req| {
        let html = include_str!("index.html");
        let headers = [("Content-Type", "text/html")];
        let mut response = req.into_response(200, Some("OK"), &headers)?;
        response.write_all(html.as_bytes())?;
        Ok::<(), anyhow::Error>(())
    })?;

    // Serve CSS at /style.css
    server.fn_handler("/style.css", esp_idf_svc::http::Method::Get, |req| {
        let css = include_str!("style.css");
        let headers = [("Content-Type", "text/css")];
        let mut response = req.into_response(200, Some("OK"), &headers)?;
        response.write_all(css.as_bytes())?;
        Ok::<(), anyhow::Error>(())
    })?;

    info!("HTTP server started!");
    info!(
        "Open http://{} in your browser to see the camera",
        ip_info.ip
    );

    // Keep the server running
    loop {
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
}
