// This file defines the ESP32 no_std Embassy application entry point.
// - Initializes hardware, RTOS support, heap allocation, WiFi, and the network stack.
// - Connects to WiFi using credentials from `credentials.rs` and serves HTTP on port 8080.
// - Serves embedded `index.html` and `style.css`, plus `/api/count` JSON for button presses.
// - Monitors GPIO6 as a pull-up button input and tracks presses with an atomic counter.
// - Depends on esp-hal, esp-radio, embassy-executor, embassy-net, and embassy-time.

// How to set up and run:
// - Set SSID and PASSWORD env variable
// - Set STATIC_IP and GATEWAY_IP env variable (e.g. "192.168.2.191" / "192.168.2.1")
// - Might be necessary to configure your WiFi access point accordingly
// - Uses the given static IP
// - Responds with some HTML content when connecting to port 8080

#![no_std]
#![no_main]

use core::sync::atomic::{AtomicU32, Ordering};
use embassy_executor::Spawner;
use embassy_net::{Runner, Stack, StackResources, tcp::TcpSocket};
use embassy_time::{Duration, Timer};
use esp_alloc as _;
use esp_backtrace as _;
#[cfg(target_arch = "riscv32")]
use esp_hal::interrupt::software::SoftwareInterruptControl;
use esp_hal::{
    clock::CpuClock,
    gpio::{Input, InputConfig, Pull},
    rng::Rng,
    time,
    timer::timg::TimerGroup,
};
use esp_println::println;
use esp_radio::{
    Controller,
    wifi::{
        ClientConfig, ModeConfig, ScanConfig, WifiController, WifiDevice, WifiEvent, WifiStaState,
    },
};

esp_bootloader_esp_idf::esp_app_desc!();

// When you are okay with using a nightly compiler it's better to use https://docs.rs/static_cell/2.1.0/static_cell/macro.make_static.html
// Convenience macro that allocates a value of type `$t` in a static `StaticCell` and
// returns a `&'static mut` reference. Avoids repeating boilerplate for every long-lived allocation.
macro_rules! mk_static {
    ($t:ty,$val:expr) => {{
        static STATIC_CELL: static_cell::StaticCell<$t> = static_cell::StaticCell::new();
        #[deny(unused_attributes)]
        let x = STATIC_CELL.uninit().write(($val));
        x
    }};
}

include!("./env.rs");
// Environment variables are not committed to the repository for security reasons
// It should be structured as like this:

//pub const WIFI_SSID: &str = "<WIFI_NAME>";
//pub const WIFI_PASSWORD: &str = "<WIFI_PASSWORD>";

// Include HTML and CSS files at compile time
const INDEX_HTML: &str = include_str!("index.html");
const STYLE_CSS: &str = include_str!("style.css");

// Network configuration
const STATIC_IP: [u8; 4] = [192, 168, 33, 100]; // Your device IP
const GATEWAY: [u8; 4] = [192, 168, 33, 4]; // Your router IP

// Global button click counter (thread-safe atomic)
static BUTTON_CLICK_COUNT: AtomicU32 = AtomicU32::new(0);

// ============================================================================
// BUTTON MONITORING TASK
// ============================================================================

/// Async task that monitors button on GPIO6 and prints "Hello" when pressed
#[embassy_executor::task]
async fn button_monitor_task(button_pin: Input<'static>) {
    println!("Button monitor task started");
    println!(
        "Initial pin state: is_low={} is_high={}",
        button_pin.is_low(),
        button_pin.is_high()
    );

    let mut button_was_pressed = false;

    loop {
        let button_is_pressed = button_pin.is_low();

        // Detect button press edge (transition from not pressed to pressed)
        if button_is_pressed && !button_was_pressed {
            // Button was just pressed - increment counter
            let count = BUTTON_CLICK_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
            let current_time = time::Instant::now().duration_since_epoch().as_millis();
            println!(
                "Button pressed! Count: {} - Time: {} ms",
                count, current_time
            );
            button_was_pressed = true;
            // Add delay after press to prevent any re-triggering
            Timer::after(Duration::from_millis(50)).await;
        } else if !button_is_pressed && button_was_pressed {
            // Button was just released - reset the state after a debounce delay
            Timer::after(Duration::from_millis(200)).await;
            button_was_pressed = false;
        }

        // Small delay to avoid overwhelming the CPU
        Timer::after(Duration::from_millis(30)).await;
    }
}

// ============================================================================
// WIFI CONNECTION TASK
// ============================================================================

/// Async task that manages WiFi connection
#[embassy_executor::task]
async fn wifi_connection_task(mut controller: WifiController<'static>) {
    println!("WiFi connection task started");
    println!("Device capabilities: {:?}", controller.capabilities());

    loop {
        if esp_radio::wifi::sta_state() == WifiStaState::Connected {
            // Wait until we're no longer connected
            controller.wait_for_event(WifiEvent::StaDisconnected).await;
            Timer::after(Duration::from_millis(5000)).await;
        }

        if !matches!(controller.is_started(), Ok(true)) {
            let client_config = ModeConfig::Client(
                ClientConfig::default()
                    .with_ssid(WIFI_SSID.into())
                    .with_password(WIFI_PASSWORD.into()),
            );
            controller.set_config(&client_config).unwrap();
            println!("Starting wifi");
            controller.start_async().await.unwrap();
            println!("Wifi started!");

            println!("Scanning for networks...");
            let scan_config = ScanConfig::default().with_max(20);
            let result = controller
                .scan_with_config_async(scan_config)
                .await
                .unwrap();
            for ap in result {
                println!("  SSID: {} | Signal: {} dBm", ap.ssid, ap.signal_strength);
                if ap.ssid == WIFI_SSID {
                    println!("  >>> Found target network!");
                }
            }
        }

        println!("Connecting to WiFi...");
        match controller.connect_async().await {
            Ok(_) => println!("WiFi connected!"),
            Err(e) => {
                println!("Failed to connect to wifi: {e:?}");
                Timer::after(Duration::from_millis(5000)).await;
            }
        }
    }
}

// ============================================================================
// NETWORK STACK TASK
// ============================================================================

/// Async task that runs the embassy-net network stack
#[embassy_executor::task]
async fn net_task(mut runner: Runner<'static, WifiDevice<'static>>) {
    runner.run().await
}

// ============================================================================
// HTTP SERVER TASK
// ============================================================================

/// Async task that runs the HTTP server using embassy-net (fully async!)
#[embassy_executor::task]
async fn http_server_task(stack: Stack<'static>) {
    println!("HTTP server task started");

    // Wait for network to be ready
    loop {
        if stack.is_link_up() {
            break;
        }
        Timer::after(Duration::from_millis(500)).await;
    }

    println!("Waiting for network configuration...");
    loop {
        if let Some(config) = stack.config_v4() {
            println!(
                "Network ready! IP: {} - Server running on http://{}:8080/",
                config.address.address(),
                config.address.address()
            );
            break;
        }
        Timer::after(Duration::from_millis(500)).await;
    }

    let mut rx_buffer = [0; 4096];
    let mut tx_buffer = [0; 4096];

    loop {
        let mut socket = TcpSocket::new(stack, &mut rx_buffer, &mut tx_buffer);
        socket.set_timeout(Some(Duration::from_secs(10)));

        // Wait for incoming connection (don't log every accept to reduce spam)
        if let Err(e) = socket.accept(8080).await {
            println!("Accept error: {:?}", e);
            continue;
        }

        // Handle the HTTP request
        if let Err(e) = handle_http_request_async(&mut socket).await {
            println!("Error handling request: {:?}", e);
        }

        socket.close();
        Timer::after(Duration::from_millis(100)).await;
    }
}

/// Async function to handle HTTP requests
async fn handle_http_request_async(
    socket: &mut TcpSocket<'_>,
) -> Result<(), embassy_net::tcp::Error> {
    use embedded_io_async::Write;

    let mut buffer = [0u8; 1024];
    let mut pos = 0;

    // Read HTTP request
    loop {
        let n = socket.read(&mut buffer[pos..]).await?;
        if n == 0 {
            return Ok(());
        }
        pos += n;

        // Check if we received complete HTTP request
        let request = core::str::from_utf8(&buffer[..pos]).unwrap_or("");
        if request.contains("\r\n\r\n") {
            break;
        }

        if pos >= buffer.len() {
            break;
        }
    }

    // Parse request to get the path
    let request_str = core::str::from_utf8(&buffer[..pos]).unwrap_or("");
    let path = extract_request_path(request_str);

    // Route based on path
    match path {
        "/" => {
            // Serve index.html (close connection for page loads)
            let response_header = "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nConnection: close\r\n\r\n";
            socket.write_all(response_header.as_bytes()).await?;
            socket.write_all(INDEX_HTML.as_bytes()).await?;
        }
        "/style.css" => {
            // Serve style.css (close connection for page loads)
            let response_header = "HTTP/1.1 200 OK\r\nContent-Type: text/css; charset=utf-8\r\nConnection: close\r\n\r\n";
            socket.write_all(response_header.as_bytes()).await?;
            socket.write_all(STYLE_CSS.as_bytes()).await?;
        }
        "/api/count" => {
            // API endpoint - return button click count as JSON
            // Use keep-alive to reuse connection for polling
            let count = BUTTON_CLICK_COUNT.load(Ordering::Relaxed);
            let response_header = "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n";
            socket.write_all(response_header.as_bytes()).await?;

            // Format JSON response manually (no std library)
            let mut json_buffer = [0u8; 64];
            let json = format_count_json(count, &mut json_buffer);
            socket.write_all(json.as_bytes()).await?;
            // Don't log every API call to reduce spam
        }
        _ => {
            // 404 Not Found
            let response = "HTTP/1.1 404 Not Found\r\nContent-Type: text/plain\r\nConnection: close\r\n\r\n404 - Not Found";
            socket.write_all(response.as_bytes()).await?;
        }
    }

    socket.flush().await?;
    println!("Request of {} handled", path);
    Ok(())
}

/// Extract the request path from an HTTP request
fn extract_request_path(request: &str) -> &str {
    // HTTP request format: "GET /path HTTP/1.1\r\n..."
    if let Some(first_line) = request.split("\r\n").next() {
        let mut parts = first_line.split_whitespace();
        // Skip method (GET, POST, etc.)
        parts.next();
        // Return path
        if let Some(path) = parts.next() {
            return path;
        }
    }
    "/"
}

/// Format button count as JSON string
fn format_count_json(count: u32, buffer: &mut [u8]) -> &str {
    use core::fmt::Write;

    // Simple JSON formatter using a cursor
    struct Cursor<'a> {
        buffer: &'a mut [u8],
        pos: usize,
    }

    impl<'a> Write for Cursor<'a> {
        fn write_str(&mut self, s: &str) -> core::fmt::Result {
            let bytes = s.as_bytes();
            let remaining = self.buffer.len() - self.pos;
            if bytes.len() > remaining {
                return Err(core::fmt::Error);
            }
            self.buffer[self.pos..self.pos + bytes.len()].copy_from_slice(bytes);
            self.pos += bytes.len();
            Ok(())
        }
    }

    let mut cursor = Cursor { buffer, pos: 0 };
    let _ = write!(cursor, "{{\"count\":{}}}", count);

    core::str::from_utf8(&cursor.buffer[..cursor.pos]).unwrap_or("{\"count\":0}")
}

// ============================================================================
// MAIN ENTRY POINT
// ============================================================================

// NOTE: esp_rtos is not FreeRTOS
// It's a custom RTOS (minimal runtime layer) built on top of the timer and software interrupt features of the ESP32.
// It provides task scheduling and async/await support, but does not use the FreeRTOS API directly.
// This esp_rtos::main attribute sets up the necessary hardware and starts the async executor for our tasks.

/// Application entry point: initialises all hardware and spawns the four concurrent tasks
/// (button monitor, WiFi connection manager, network stack runner, HTTP server).
/// Runs in an idle loop afterwards to keep the executor alive.
#[esp_rtos::main]
async fn main(spawner: Spawner) -> ! {
    // ============================================================================
    // HARDWARE INITIALIZATION
    // ============================================================================

    esp_println::logger::init_logger_from_env();
    let config = esp_hal::Config::default().with_cpu_clock(CpuClock::max());
    let peripherals = esp_hal::init(config);

    println!("Init!");

    // Allocate 72KB heap for dynamic memory allocation
    esp_alloc::heap_allocator!(size: 72 * 1024);

    // Initialize timer and RTOS
    let timg0 = TimerGroup::new(peripherals.TIMG0);
    #[cfg(target_arch = "riscv32")]
    let sw_int = SoftwareInterruptControl::new(peripherals.SW_INTERRUPT);
    esp_rtos::start(
        timg0.timer0,
        #[cfg(target_arch = "riscv32")]
        sw_int.software_interrupt0,
    );

    // 1. Setup button
    let button_config = InputConfig::default().with_pull(Pull::Up);
    let button_pin = Input::new(peripherals.GPIO6, button_config);

    // 2. Setup WIFI and network stack using esp-radio and embassy-net
    let esp_radio_ctrl = mk_static!(Controller<'static>, esp_radio::init().unwrap());
    let (controller, interfaces) =
        esp_radio::wifi::new(esp_radio_ctrl, peripherals.WIFI, Default::default()).unwrap();

    let wifi_device = interfaces.sta;
    println!("ESP32 MAC Address: {:02X?}", wifi_device.mac_address());

    // 3. Configure network stack with static IP
    let net_config = embassy_net::Config::ipv4_static(embassy_net::StaticConfigV4 {
        address: embassy_net::Ipv4Cidr::new(
            embassy_net::Ipv4Address::new(STATIC_IP[0], STATIC_IP[1], STATIC_IP[2], STATIC_IP[3]),
            24,
        ),
        gateway: Some(embassy_net::Ipv4Address::new(
            GATEWAY[0], GATEWAY[1], GATEWAY[2], GATEWAY[3],
        )),
        dns_servers: Default::default(),
    });

    println!(
        "Configuring static IP: {}.{}.{}.{}",
        STATIC_IP[0], STATIC_IP[1], STATIC_IP[2], STATIC_IP[3]
    );
    println!(
        "Gateway: {}.{}.{}.{}",
        GATEWAY[0], GATEWAY[1], GATEWAY[2], GATEWAY[3]
    );

    let rng = Rng::new();
    let seed = (rng.random() as u64) << 32 | rng.random() as u64;

    // 4. Initialize embassy-net stack
    let (stack, runner) = embassy_net::new(
        wifi_device,
        net_config,
        mk_static!(StackResources<3>, StackResources::<3>::new()),
        seed,
    );

    // 5. Spawn async tasks for button monitoring, WiFi connection management, network stack, and HTTP server
    spawner.spawn(button_monitor_task(button_pin)).ok();
    spawner.spawn(wifi_connection_task(controller)).ok();
    spawner.spawn(net_task(runner)).ok();
    spawner.spawn(http_server_task(stack)).ok();

    // 6. Start main loop
    println!("All tasks spawned, entering main loop");

    loop {
        println!("Main loop tick");
        Timer::after(Duration::from_millis(1000)).await;
    }
}
