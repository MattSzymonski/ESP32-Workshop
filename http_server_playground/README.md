# ESP32 Rust HTTP Server

A web server running on an **ESP32-C6-DevKitC-1** board, written in Rust using the `esp-idf-svc` ecosystem.  
After boot it connects to Wi-Fi and serves a control dashboard at `http://<device-ip>/`.  
Each hardware peripheral is registered as an independent module that contributes both an API endpoint and an HTML card to the dashboard.

---

## Requirements

| Tool                                           | Purpose                               |
| ---------------------------------------------- | ------------------------------------- |
| [Rust](https://rustup.rs) ≥ 1.82               | Compiler                              |
| [espup](https://github.com/esp-rs/espup)       | ESP32 Rust toolchain installer        |
| [espflash](https://github.com/esp-rs/espflash) | Flash and monitor tool                |
| [ldproxy](https://github.com/esp-rs/embuild)   | Linker proxy (installed with `espup`) |

Install the ESP32 toolchain:

```sh
espup install
```

---

## Configuration

Create `src/env.rs` (not committed — listed in `.gitignore`):

```rust
pub const WIFI_SSID: &str = "<YOUR_WIFI_SSID>";
pub const WIFI_PASSWORD: &str = "<YOUR_WIFI_PASSWORD>";
```

---

## Build & Flash

```sh
# Build for the ESP32-C6 target
cargo build --release

# Flash and open the serial monitor
espflash flash --monitor --port COM3 D:\cargo-target\http_server_playground\riscv32imac-esp-espidf\release
```

> **Windows note:** Build output is redirected to `D:\cargo-target\http_server_playground` and the ESP-IDF toolchain to `C:\esp\tools` (configured in `.cargo\config.toml`) to avoid the Windows 260-character `MAX_PATH` limit imposed by ESP-IDF's deeply nested CMake build tree.

After boot the serial monitor prints the assigned IP address:

```text
I (...) http_server_playground: Open: http://192.168.x.x
```

Open that URL in a browser to access the dashboard.

---

## Project Structure

```text
src/
  main.rs           — Wi-Fi setup, HTTP server init, idle loop
  env.rs            — Wi-Fi credentials (not committed)
  index.html        — Dashboard HTML template ({{MODULES}} placeholder)
  style.css         — Dashboard styles
  led/
    mod.rs          — WS2812 RGB LED module
    card.html       — Dashboard card HTML for the LED
  servo/
    mod.rs          — SG90 micro servo module
    card.html       — Dashboard card HTML for the servo
  display/
    mod.rs          — ST7735S SPI TFT display module
    card.html       — Dashboard card HTML for the display
```

Each module exposes a single:

```rust
pub fn register(server, ...peripherals...) -> anyhow::Result<String>
```

function that:

1. Initialises the hardware driver
2. Registers one or more HTTP API endpoints on the server
3. Returns an HTML card string (from `card.html`) to be embedded in the dashboard

To add a new module, create a new subdirectory following the same pattern, call `register()` in `main.rs`, and concatenate the returned HTML card into `modules_html`.

---

## Modules

### WS2812 LED (`src/led/`)

Controls the onboard addressable RGB LED via the ESP32 **RMT** peripheral.

| Property | Value                                                |
| -------- | ---------------------------------------------------- |
| GPIO     | **GPIO8** (onboard on ESP32-C6-DevKitC-1)            |
| Protocol | WS2812 / NeoPixel — single-wire timed pulses via RMT |
| Driver   | `ws2812-esp32-rmt-driver`                            |

### API endpoint

```text
GET /api/blink
```

Turns the LED **white** (`R=50 G=50 B=50`) for 1 second, then turns it **off**.

Response:

```json
{"blinked":true}
```

### Wiring

The WS2812 LED is **built into the ESP32-C6-DevKitC-1 board** — no external wiring needed.  
GPIO8 drives the LED data line internally.

---

### SG90 Servo (`src/servo/`)

Controls a **Tower Pro Micro Servo 9g SG90** via the ESP32 **LEDC** (PWM) peripheral.

| Property   | Value                                                    |
| ---------- | -------------------------------------------------------- |
| GPIO       | **GPIO4**                                                |
| Protocol   | 50 Hz PWM — 0.5 ms pulse = 0°, 2.5 ms pulse = 180°       |
| Resolution | 14-bit (16383 ticks / 20 ms period); duty range 410–2048 |
| Driver     | `esp-idf-svc` LEDC driver                                |

### API endpoint

```text
GET /api/servo?angle=<0-180>
```

Moves the servo to the specified angle and holds it there.

Response:

```json
{"angle":90,"duty":1228}
```

### Wiring

The SG90 has three wires:

| Wire colour         | Connect to                                     |
| ------------------- | ---------------------------------------------- |
| **Brown** (GND)     | Any **GND** pin on the ESP32 board             |
| **Red** (VCC)       | **5V** pin on the ESP32 board (`5V0` / `VBUS`) |
| **Orange** (Signal) | **GPIO4**                                      |

> **Note:** The SG90 requires 5 V on its power rail. The signal wire is 3.3 V tolerant so it can be driven directly from the ESP32 GPIO without a level shifter.

```text
ESP32-C6-DevKitC-1        SG90 Servo
┌─────────────────┐       ┌─────────────────┐
│             GND ├───────┤ Brown  (GND)    │
│             5V0 ├───────┤ Red    (VCC)    │
│           GPIO4 ├───────┤ Orange (Signal) │
└─────────────────┘       └─────────────────┘
```

---

### ST7735S Display (`src/display/`)

Controls a **160×80 ST7735S SPI TFT display** via the ESP32 **SPI2** peripheral.

| Property       | Value                    |
| -------------- | ------------------------ |
| SCLK           | **GPIO10**               |
| MOSI           | **GPIO11**               |
| CS             | **GPIO18**               |
| DC             | **GPIO5**                |
| RST            | **GPIO6**                |
| BL (backlight) | **GPIO7**                |
| SPI speed      | 4 MHz                    |
| Driver         | `st7735-lcd`             |
| Orientation    | Landscape, offset (0,24) |

### API endpoints

```text
GET /api/display/text?msg=<percent-encoded text>&color=<RRGGBB>
```

Clears the screen to black and draws one line of text in the requested colour.

Response:

```json
{"ok":true}
```

```text
GET /api/display/clear?color=<RRGGBB>
```

Fills the entire screen with a solid colour (default `000000` = black).

Response:

```json
{"ok":true}
```

### Wiring

| ST7735S pin | Connect to |
| ----------- | ---------- |
| VCC         | **3V3**    |
| GND         | **GND**    |
| SCL         | **GPIO10** |
| SDA         | **GPIO11** |
| CS          | **GPIO18** |
| DC          | **GPIO5**  |
| RES         | **GPIO6**  |
| BLK         | **GPIO7**  |

```text
ESP32-C6-DevKitC-1        ST7735S display
┌─────────────────┐       ┌──────────────────┐
│             3V3 ├───────┤ VCC              │
│             GND ├───────┤ GND              │
│          GPIO10 ├───────┤ SCL / CLK        │
│          GPIO11 ├───────┤ SDA / MOSI       │
│          GPIO18 ├───────┤ CS               │
│           GPIO5 ├───────┤ DC               │
│           GPIO6 ├───────┤ RES / RST        │
│           GPIO7 ├───────┤ BLK / LED        │
└─────────────────┘       └──────────────────┘
```

---

## Dashboard

The dashboard at `http://<device-ip>/` is assembled at boot by substituting module HTML cards into the `{{MODULES}}` placeholder in `src/index.html`.

Additional endpoints always available:

| Endpoint         | Description                      |
| ---------------- | -------------------------------- |
| `GET /`          | Dashboard HTML                   |
| `GET /style.css` | Stylesheet                       |
| `GET /health`    | Health check — `{"status":"ok"}` |