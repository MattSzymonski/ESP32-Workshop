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
# Build for the ESP32-C6 target with all modules enabled (default)
cargo build --release

# Or build a slim binary with only the modules you need (see Features below)
cargo build --release --no-default-features --features "led,gamepad"

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

## Features

Every hardware module is gated behind its own Cargo feature so you can compile a slimmer binary that only contains what you need. Disabled modules are not compiled at all — no code, no static framebuffers, no background threads. The big win is in DRAM at runtime, which matters because the ESP32-C6 only has ~336 KiB of free heap after the IDF starts up and a fully-loaded build can leave less than 80 KiB of contiguous block free.

| Feature      | Module                                |
| ------------ | ------------------------------------- |
| `led`        | WS2812 onboard RGB LED                |
| `servo`      | SG90 micro servo                      |
| `solar`      | Solar panel ADC                       |
| `button`     | Button press counter                  |
| `buzzer`     | PWM buzzer                            |
| `ultrasonic` | HC-SR04 distance sensor               |
| `joystick`   | 2-axis joystick + push button         |
| `display`    | ST7735S TFT with renderer sub-modules |
| `gamepad`    | BLE HID gamepad (8BitDo Ultimate 2C)  |

The `default` feature set enables everything. Disable defaults with `--no-default-features` and add only what you want:

```sh
# Minimal build: just the LED and the gamepad over BLE
cargo build --release --no-default-features --features "led,gamepad"
```

When the `gamepad` feature is disabled the BLE controller is not initialised at all, freeing ~30 KiB of contiguous DRAM otherwise reserved by NimBLE.

---

## Project Structure

```text
src/
  main.rs           — Wi-Fi setup, HTTP server init, idle loop
  mem.rs            — Heap diagnostics (`mem::report(label)`) for OOM debugging
  env.rs            — Wi-Fi credentials (not committed)
  index.html        — Dashboard HTML template ({{MODULES}} placeholder)
  style.css         — Dashboard styles
  led/              — WS2812 RGB LED module
  servo/            — SG90 micro servo module
  solar/            — Solar panel ADC module
  button/           — Button press counter module
  buzzer/           — Buzzer module
  ultrasonic/       — HC-SR04 ultrasonic distance sensor module
  joystick/         — Joystick module (VRX/VRY ADC + SW button)
  gamepad/          — BLE HID gamepad module (8BitDo Ultimate 2C)
  display/          — ST7735S SPI TFT display module
    basic/          —   text / clear sub-mode
    wireframe_renderer/  —   software 3-D wireframe sub-mode
    shader_renderer/     —   software shader sub-mode
```

Each module exposes a `register(server, ...peripherals...)` function (gated on its Cargo feature) that:

1. Initialises the hardware driver
2. Registers one or more HTTP API endpoints on the server
3. Returns an HTML card string (from `card.html`) to be embedded in the dashboard

The dashboard is **streamed** rather than rendered into one big string — the template is split once at the `{{MODULES}}` placeholder, each module card is kept as its own `Arc<str>`, and the HTTP handler writes them prefix → cards → suffix. This avoids a ~57 KiB contiguous allocation that would fail when DRAM is fragmented.

To add a new module, create a subdirectory following the same pattern, gate `mod xxx;` behind a feature, and conditionally call `register()` from `main.rs`.

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

### Joystick (`src/joystick/`)

Reads a two-axis joystick module (e.g. KY-023) via ADC and a digital input.

| Property  | Value                                                            |
| --------- | ---------------------------------------------------------------- |
| VRX pin   | **GPIO0** — horizontal axis, ADC1 channel 0 (12-bit raw, 0–4095) |
| VRY pin   | **GPIO3** — vertical axis, ADC1 channel 3 (12-bit raw, 0–4095)   |
| SW pin    | **GPIO20** — push-button, active-low, internal pull-up           |
| ADC range | DB\_12 attenuation — 0–3.3 V, full 0–4095 count range            |
| SW poll   | Background thread, 50 ms interval (debounce)                     |
| Driver    | `esp-idf-svc` ADC oneshot driver (shared with solar module)      |

> **Note:** GPIO0–6 are the only ADC-capable pins on ESP32-C6. GPIO19/GPIO20 (the
> physically convenient pins) have **no ADC**. GPIO0 and GPIO3 are the two free
> ADC1 pins not used by other modules. The ADC1 hardware driver is created once
> in `main.rs` and shared between solar and joystick via `Arc<AdcDriver>`.

> **Note:** GPIO15 is already the buzzer signal pin, so GPIO20 is used for SW.

### API endpoint

```text
GET /api/joystick
```

Returns:
```json
{"x": 2048, "y": 2048, "sw": false}
```

| Field | Description                                                        |
| ----- | ------------------------------------------------------------------ |
| `x`   | VRX raw ADC count 0–4095 (left=0, right=4095 — direction may vary) |
| `y`   | VRY raw ADC count 0–4095 (up=0, down=4095 — direction may vary)    |
| `sw`  | `true` for 200 ms after each button press, then `false`            |

The dashboard card shows a live SVG ring with a circle inside that moves with
the stick position, updating every 150 ms. A small ring on the right fills
green for 200 ms whenever the button is pressed.

### Wiring

| Joystick pin | Connect to                                                    |
| ------------ | ------------------------------------------------------------- |
| **GND**      | **GND**                                                       |
| **+5V**      | **3V3** (KY-023 works at 3.3 V; ADC input stays within range) |
| **VRX**      | **GPIO0**                                                     |
| **VRY**      | **GPIO3**                                                     |
| **SW**       | **GPIO20**                                                    |

---

### Ultrasonic Distance Sensor (`src/ultrasonic/`)

Measures distance using an **HC-SR04** (or compatible) ultrasonic sensor via two GPIO pins.

| Property | Value                                                |
| -------- | ---------------------------------------------------- |
| TRIG pin | **GPIO22** — 10 µs output pulse to start measurement |
| ECHO pin | **GPIO21** — input, pulse width ∝ distance           |
| Range    | ~2 cm – 400 cm                                       |
| Formula  | `d = echo_µs × 0.0343 / 2` (cm, sound at 20 °C)      |
| Timeout  | 30 ms (guards against no-echo / out-of-range)        |
| Driver   | `esp-idf-svc` GPIO input / output driver             |

### API endpoint

```text
GET /api/ultrasonic
```

On success:
```json
{"distance_cm": 23.4, "echo_us": 1365}
```

If no echo is received within the timeout (object out of range or sensor disconnected):
```json
{"error": "no echo"}
```

The dashboard card shows the latest reading in cm, the raw echo duration in µs, a one-shot **Measure** button, and an **Auto** toggle that polls every 2 seconds.

### How it works

1. GPIO22 is pulled HIGH for 10 µs (trigger pulse) using `esp_rom_delay_us` for precise timing.
2. The firmware waits for GPIO21 (ECHO) to go HIGH — the HC-SR04 raises it when the ultrasound burst is sent.
3. The HIGH duration of ECHO is measured with `std::time::Instant`.
4. Distance is calculated: `distance_cm = echo_µs × 0.0343 / 2`.

### Wiring

| HC-SR04 pin | Connect to                                                                                                              |
| ----------- | ----------------------------------------------------------------------------------------------------------------------- |
| **VCC**     | **5V0** (the sensor requires 5 V; ECHO output is 5 V — use a resistor divider to bring it to 3.3 V for the ESP32 input) |
| **GND**     | **GND**                                                                                                                 |
| **TRIG**    | **GPIO22**                                                                                                              |
| **ECHO**    | **GPIO21** via voltage divider (e.g. 1 kΩ + 2 kΩ → 3.3 V at GPIO)                                                       |

```text
ESP32-C6                 HC-SR04
┌─────────────────┐      ┌──────────────┐
│             5V0 ├──────┤ VCC          │
│             GND ├──────┤ GND          │
│          GPIO22 ├──────┤ TRIG         │
│                 │      │ ECHO ──┬─────┐
│          GPIO21 ├──R2──┤              │  R1
└─────────────────┘      └──────────────┘  └─ GND
                            R1=1kΩ, R2=2kΩ
```

> **Caution:** The HC-SR04 ECHO pin outputs **5 V**. Connect it to GPIO21 through a resistor divider (1 kΩ / 2 kΩ) to limit the input to 3.3 V, otherwise the ESP32 GPIO may be damaged.

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
| RES         | **GPIO6**  |
| DC          | **GPIO5**  |
| CS          | **GPIO18** |
| BLK         | **GPIO7**  |

```text
ESP32-C6-DevKitC-1        ST7735S display
┌─────────────────┐       ┌──────────────────┐
│             3V3 ├───────┤ VCC              │
│             GND ├───────┤ GND              │
│          GPIO10 ├───────┤ SCL / CLK        │
│          GPIO11 ├───────┤ SDA / MOSI       │
│           GPIO6 ├───────┤ RES / RST        │
│           GPIO5 ├───────┤ DC               │
│          GPIO18 ├───────┤ CS               │
│           GPIO7 ├───────┤ BLK / LED        │
└─────────────────┘       └──────────────────┘
```

---

### BLE HID Gamepad (`src/gamepad/`)

Connects to a Bluetooth Low Energy HID gamepad — primarily targeted at the **8BitDo Ultimate 2C** in **B (Bluetooth)** mode (slide-switch position B). No GPIO pins are used; the controller is paired over the on-board BLE radio via the **NimBLE** stack (the `esp32-nimble` crate).

| Property       | Value                                                |
| -------------- | ---------------------------------------------------- |
| GPIO           | none — uses the on-board BLE radio                   |
| BLE service    | HID-over-GATT (`0x1812`)                             |
| Pairing        | Just-Works (NoInputNoOutput, bonded)                 |
| Auto-reconnect | yes — outer loop re-scans on disconnect              |
| Coexists with  | Wi-Fi (the C6 modem is shared between Wi-Fi and BLE) |

The module spawns a dedicated `gamepad-ble` thread (BLE objects are `!Send`), scans for any device whose advertised name contains `8BitDo`, `Ultimate`, `Controller`, or `Gamepad`, connects, subscribes to every notifying HID Report characteristic, and forwards the latest input report bytes to the dashboard. It also remembers any **writable** Report characteristics and uses them to deliver vibration commands.

> **Init order matters.** The BLE controller must be initialised *before* Wi-Fi. Wi-Fi grabs ~80 KiB of contiguous DRAM the moment `EspWifi::new` is called, leaving the heap too fragmented for `r_ble_controller_init`'s ~30 KiB allocation. `main.rs` calls `BLEDevice::take()` first to claim that block while the heap is still pristine. The actual scan/connect state machine is started later, after the page parts are built, so NimBLE's mbuf allocations don't fragment the page-streaming buffers.

### API endpoints

```text
GET /api/gamepad
```

Returns a JSON snapshot of the latest input report:

```json
{
  "connected": true,
  "name":      "8BitDo Ultimate 2C",
  "addr":      "...",
  "ts":        1234567,
  "report":    "<hex bytes>",
  "reportMap": "<hex bytes — HID descriptor, read once>"
}
```

The browser-side card decodes `report` into 4 axes, 2 analog triggers, a D-pad hat, and 11 buttons (A, B, X, Y, LB, RB, View, Menu, L3, R3, Home). It auto-detects the layout from the report length:

| Length     | Layout                                                   | Axes / triggers               | Buttons               |
| ---------- | -------------------------------------------------------- | ----------------------------- | --------------------- |
| 7–11 bytes | Android-style (default for 8BitDo Ultimate 2C in B mode) | 8-bit unsigned, center `0x80` | bytes 7 + 8 bitmaps   |
| ≥ 12 bytes | Xbox One BLE HID                                         | 16-bit LE, center `0x8000`    | bytes 13 + 14 bitmaps |

The card always shows the raw bytes at the bottom for diagnostics, so if your specific firmware uses a different layout you can re-derive the bit assignments quickly.

```text
GET /api/gamepad/vibrate
```

Triggers a 1-second rumble on both motors. Sends the standard 8-byte **Xbox-style BLE rumble output report** (`[0x0F, 0, 0, 80, 80, 100, 0, 0]`) to every writable HID Report characteristic on the device. The 8BitDo Ultimate 2C in B mode accepts this format. If the controller does not expose a writable Report or uses a different rumble protocol, the write either fails silently or has no effect — the request is one-shot fire-and-forget.

Response:

```json
{"ok":true}
```

### Pairing / wiring

There is no wiring. To pair:

1. Slide the mode switch on the back of the **8BitDo Ultimate 2C** to position **B**.
2. Hold the **Start** button until the LED ring starts pulsing — the controller is in BLE pairing mode.
3. Power the ESP32. Within a few seconds the dashboard's Gamepad card should turn green and show the controller name.

After the first pairing the controller is bonded and reconnects automatically on subsequent boots.

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

The dashboard at `http://<device-ip>/` is assembled at boot by streaming the `index.html` template, each enabled module's HTML card, and the template suffix as separate response chunks (no single-big-string concatenation).

Additional endpoints always available:

| Endpoint         | Description                      |
| ---------------- | -------------------------------- |
| `GET /`          | Dashboard HTML                   |
| `GET /style.css` | Stylesheet                       |
| `GET /health`    | Health check — `{"status":"ok"}` |