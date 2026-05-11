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
  solar/
    mod.rs          — Solar panel ADC module
    card.html       — Dashboard card HTML for the solar panel
  button/
    mod.rs          — Button press counter module
    card.html       — Dashboard card HTML for the button
  buzzer/
    mod.rs          — Buzzer module
    card.html       — Dashboard card HTML for the buzzer
  display/
    mod.rs          — ST7735S SPI TFT display module (parent + mode switch)
    card.html       — Dashboard card HTML for the display
    basic/
      mod.rs        — Text / clear sub-mode
      card.html     — Sub-card HTML
    renderer/
      mod.rs        — Software 3-D renderer sub-mode
      card.html     — Sub-card HTML
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
---

### Button (`src/button/`)

Counts presses of a button via the ESP32-C6 **GPIO** input peripheral.

| Property  | Value                                                 |
| --------- | ----------------------------------------------------- |
| GPIO      | **GPIO9** (onboard BOOT button on ESP32-C6-DevKitC-1) |
| Logic     | Active-low, internal pull-up enabled                  |
| Detection | Background polling thread, 50 ms interval (debounce)  |
| Driver    | `esp-idf-svc` GPIO input driver                       |

### API endpoints

```text
GET /api/button        → {"count": N}    returns current press count
GET /api/button/reset  → {"count": 0}    resets count to zero
```

The dashboard card shows a large live counter (auto-refreshes every 500 ms) and a Reset button.

### Notes

- **GPIO9 / BOOT button:** this is the onboard button already present on the ESP32-C6-DevKitC-1 board. No external wiring is required. An external button wired between GPIO9 and GND also works.
- The strapping level on GPIO9 is only sampled by the ROM bootloader during reset. After boot it becomes a regular GPIO input and can be used freely.
- The 50 ms polling interval acts as implicit debounce. Bounces shorter than 50 ms are suppressed; presses shorter than 50 ms may occasionally be missed.

### Wiring (external button)

An external button is optional since the BOOT button is already connected:

| Button pin | Connect to |
| ---------- | ---------- |
| Pin 1      | **GPIO9**  |
| Pin 2      | **GND**    |

---

### Buzzer (`src/buzzer/`)

Drives a **passive or active buzzer** via the ESP32-C6 **LEDC** (PWM) peripheral.

| Property   | Value                                    |
| ---------- | ---------------------------------------- |
| GPIO       | **GPIO15**                               |
| Protocol   | 2 kHz PWM, 50 % duty cycle (square wave) |
| Resolution | 8-bit (255 ticks / 500 µs period)        |
| Duration   | 1–5000 ms, default 200 ms                |
| Driver     | `esp-idf-svc` LEDC driver                |

### API endpoint

```text
GET /api/beep?duration_ms=<1-5000>
```

Sounds the buzzer for the requested number of milliseconds.
The dashboard card provides a slider (50–2000 ms) and a Beep button.

Response:

```json
{"beeped":true,"duration_ms":200}
```

### Notes

- **Passive buzzer** (recommended): produces a clear 2 kHz tone driven by the PWM square wave.
- **Active buzzer**: the PWM signal still works — the buzzer will emit its built-in tone while the duty cycle is non-zero.
- The HTTP handler releases the mutex lock during the sleep phase so the server remains responsive to other requests while the buzzer is sounding.

### Wiring

| Buzzer pin     | Connect to                                    |
| -------------- | --------------------------------------------- |
| **+** (VCC)    | **3V3** or **5V0** depending on buzzer rating |
| **−** (GND)    | **GND**                                       |
| **S** (signal) | **GPIO15**                                    |

```text
ESP32-C6-DevKitC-1        Buzzer
┌─────────────────┐       ┌────────────────┐
│             3V3 ├───────┤ + (VCC)        │
│             GND ├───────┤ − (GND)        │
│          GPIO15 ├───────┤ S (signal)     │
└─────────────────┘       └────────────────┘
```

---

### Solar Panel (`src/solar/`)

Reads the solar panel output voltage via the ESP32-C6 **ADC1** peripheral.

| Property    | Value                                  |
| ----------- | -------------------------------------- |
| GPIO        | **GPIO2** (ADC1 channel 2 on ESP32-C6) |
| Attenuation | 12 dB — full-scale input range 0–3.3 V |
| Resolution  | 12-bit (raw values 0–4095)             |
| Driver      | `esp-idf-svc` ADC oneshot driver       |

### API endpoint

```text
GET /api/solar
```

Returns the current ADC reading and the corresponding voltage.

Response:

```json
{"raw":2048,"voltage_mv":1650}
```

The dashboard card auto-refreshes every 2 seconds and shows a live bar graph.
Readings below 50 mV are displayed as `—` (no signal).

### Floating pin / unconnected readings

GPIO2 picks up capacitive noise from nearby signals when nothing is connected, producing spurious readings of 400–1200 mV.
The firmware enables the ESP32’s **internal pull-down** (~45 kΩ) on GPIO2 so the pin sits at 0 V when no panel is attached.
When a solar panel is connected its output easily overrides the weak pull-down and normal measurement resumes.

> **Tip:** If you are using a voltage divider (see below), the lower resistor (e.g. 10 kΩ) also acts as a pull-down stronger than the internal one, giving even more stable zero readings in the dark.


Connect the solar panel output through a **voltage divider** if the panel voltage can exceed 3.3 V.
For a panel that stays within 3.3 V (e.g. a small 5 V panel feeding through a resistor divider):

| Solar panel wire | Connect to              |
| ---------------- | ----------------------- |
| **+** (positive) | **GPIO2** (via divider) |
| **−** (negative) | **GND**                 |

```text
Solar panel (+) ---- R1 ----+---- GPIO2
                            |
                           R2
                            |
Solar panel (-) ------------+---- GND (ESP32)
```

For example R1 = 10 kΩ, R2 = 10 kΩ gives ÷2, suitable for panels up to ~6.6 V.

> **Caution:** Never connect more than **3.3 V** directly to any ESP32 GPIO pin.

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

## Reserved GPIO pins (ESP32-C6-DevKitC-1)

The following pins are reserved by the board hardware and **must not be used as GPIO outputs**:

| GPIO       | Function                  | Consequence of misuse                                                                |
| ---------- | ------------------------- | ------------------------------------------------------------------------------------ |
| **GPIO12** | USB D− (USB Serial/JTAG)  | Drops USB connection                                                                 |
| **GPIO13** | USB D+ (USB Serial/JTAG)  | Drops USB connection / device reset                                                  |
| **GPIO1**  | UART0 TX (serial monitor) | Corrupts log output                                                                  |
| **GPIO9**  | BOOT button (active low)  | Unintended boot-mode entry — **safe to use as input** after boot (see Button module) |

> **Tip:** If the board suddenly disconnects or resets when a new GPIO output is initialised, check that the pin is not in the table above.

---

## Dashboard

The dashboard at `http://<device-ip>/` is assembled at boot by substituting module HTML cards into the `{{MODULES}}` placeholder in `src/index.html`.

Additional endpoints always available:

| Endpoint         | Description                      |
| ---------------- | -------------------------------- |
| `GET /`          | Dashboard HTML                   |
| `GET /style.css` | Stylesheet                       |
| `GET /health`    | Health check — `{"status":"ok"}` |