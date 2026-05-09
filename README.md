# ESP32 Workshop

A collection of ESP32 Rust projects exploring different levels of the embedded software stack — from bare-metal async runtimes to the full ESP-IDF framework.  
All projects target the **Seeed XIAO ESP32-C6** or **ESP32-C6-DevKitC-1** board.

---

## Projects

### [`bare-metal_http_server`](bare-metal_http_server/)

A no_std HTTP server built on raw TCP sockets using the Embassy async runtime — no FreeRTOS, no ESP-IDF.

**What it does**
- Serves a CSS-styled HTML page and a `/api/count` JSON endpoint over port 8080
- Monitors a physical push button on GPIO6 and tracks press counts with an atomic counter
- Uses a static IP address (configurable in `main.rs`)

**Stack:** `no_std` · `esp-hal` · `embassy-executor` · `embassy-net` · `esp-radio` · manual HTTP over TCP

---

### [`http_server_modules`](http_server_modules/)

A modular ESP-IDF HTTP server that serves a hardware control dashboard over Wi-Fi.

**What it does**
- Connects to Wi-Fi and starts `EspHttpServer` on the default port
- Each hardware peripheral is a self-contained module that registers its own API endpoint and contributes an HTML card to the dashboard
- Currently supports three modules:

| Module      | Hardware                               | API endpoint                                      |
| ----------- | -------------------------------------- | ------------------------------------------------- |
| **LED**     | WS2812B RGB LED on GPIO8 via RMT       | `GET /api/blink`                                  |
| **Servo**   | SG90 micro servo on GPIO4 via LEDC PWM | `GET /api/servo?angle=<0-180>`                    |
| **Display** | ST7735S 160×80 SPI TFT on SPI2         | `GET /api/display/text`, `GET /api/display/clear` |

**Stack:** `std` · `esp-idf-svc` · `EspHttpServer` · `embedded-graphics` · FreeRTOS

---

### [`http_server_with_camera`](http_server_with_camera/)

A webpage server that captures photos, displays and analyses them, and makes outbound web requests.

**Stack:** `std` · `esp-idf` · FreeRTOS

> See the [Red-Alert](https://github.com/MattSzymonski/Red-Alert) project for a full implementation reference.
